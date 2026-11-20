//! An agent's title: a few words the person knows its work by, beside its
//! name, which stays the handle. As Codex titles a thread, an agent the
//! person or a lead starts with a message (a task, a `/new` agent) shows
//! that message as it starts, flattened to a line ([`of_message`]), until
//! a model has written a title of it ([`generate`]): Codex's small model
//! where the agent runs on ChatGPT and the person's list has it, else the
//! agent's own, in a fork with no tools that takes one turn. Any other
//! agent (a helper, a task started before titles) has its first message
//! read once from its history and noted as it is. A project's lead takes
//! no title of its own: its project's, which the project file keeps where
//! the person can change it, else none, so its project's name shows. The
//! daemon keeps none: Sonny notes each beside the store, as it notes the
//! profile a task was started as.
use std::path::{Path, PathBuf};

/// The most characters a title made of a first message takes, as Codex
/// shows a thread's first message until its title is written.
pub const MOST: usize = 60;

/// The most characters a title a model writes takes, and a project's:
/// Codex's `THREAD_TITLE_MAX_CHARS`.
pub const WRITTEN: usize = 36;

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

    /// The title noted for `bot`, the agent of id `id`, the one a model
    /// wrote first: none where it was noted for another agent of that name.
    pub fn of(&self, bot: &str, id: Option<i64>) -> Option<String> {
        let id = id?;
        (self.file(&written(bot), id)).or_else(|| self.file(bot, id))
    }

    /// The title in the file `file` for the agent of id `id`.
    fn file(&self, file: &str, id: i64) -> Option<String> {
        // One read or noted here, its file written yet or not.
        let read = READ.lock().unwrap_or_else(|e| e.into_inner());
        let key = (file.to_owned(), id);
        if let Some(title) = read.get(&self.dir).and_then(|read| read.get(&key)) {
            return Some(title.clone());
        }
        drop(read);
        let names = NAMES.lock().unwrap_or_else(|e| e.into_inner());
        if !(names.get(&self.dir))
            .is_some_and(|names| names.names.contains(std::ffi::OsStr::new(file)))
        {
            return None;
        }
        drop(names);
        noted_in(&self.dir, file, id)
    }
}

/// The file a model's title of `bot` is noted in, beside the one its first
/// message made: a name of its own, `@` being in no agent's, so another
/// window sees it as the folder changes and reads it once, as it read that
/// one, never again.
fn written(bot: &str) -> String {
    format!("{bot}@")
}

/// The title of an agent whose first message is `message`, until a model
/// writes one: as Codex's `fallback_thread_title`, its first 2,000
/// characters read as Markdown, their words on one line, cut to [`MOST`]
/// with `…`.
pub fn of_message(message: &str) -> String {
    use pulldown_cmark::{Event, Parser, TagEnd};
    let excerpt: String = message.trim().chars().take(2_000).collect();
    let mut text = String::new();
    for event in Parser::new(&excerpt) {
        match event {
            Event::Text(value) | Event::Code(value) => text.push_str(&value),
            Event::SoftBreak
            | Event::HardBreak
            | Event::Rule
            | Event::End(
                TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::Item | TagEnd::CodeBlock,
            ) => text.push(' '),
            _ => {}
        }
    }
    let mut title = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = title.char_indices().skip(MOST - 1);
    if let Some((end, _)) = chars.next()
        && chars.next().is_some()
    {
        title.truncate(title[..end].trim_end().len());
        title.push('…');
    }
    title
}

/// What the model that writes a title is told, word for word as Codex
/// tells it (`thread_title_instructions`).
pub const INSTRUCTIONS: &str = concat!(
    "Generate a concise, single-line task title of at most 36 characters ",
    "and under five words where possible. Start with an imperative verb. ",
    "Capitalize only the first word unless the user's language, proper nouns, ",
    "acronyms, or code terms require otherwise. Preserve ticket references ",
    "exactly. Write in the user's language. Do not use quotes, markdown, ",
    "or trailing punctuation. Do not answer the request."
);

/// The most bytes a title's request takes, its instructions included:
/// Codex's `THREAD_TITLE_PROMPT_MAX_BYTES`.
const PROMPT_BYTES: usize = 960;

/// The model Codex writes titles with, on ChatGPT, at low effort.
const SMALL: &str = "chatgpt/gpt-5.6-luna";

/// How long a title's turn may take, as Codex gives its structured
/// request.
const DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// The title's reply, held to [`WRITTEN`] characters by the provider.
const SCHEMA: &str = r#"{"type":"object","properties":{"title":{"type":"string","minLength":1,"maxLength":36}},"required":["title"],"additionalProperties":false}"#;

const REPLY: agent_client::judge::Reply = agent_client::judge::Reply {
    schema: SCHEMA,
    parse: parse_reply,
    origin: "title",
};

/// The request for a title of `message`, as Codex's `thread_title_prompt`
/// asks it: the instructions, then the message, cut at a character to
/// [`PROMPT_BYTES`] in all.
pub fn prompt(message: &str) -> String {
    let prefix = format!("{INSTRUCTIONS}\n\nUser prompt:\n");
    let room = PROMPT_BYTES.saturating_sub(prefix.len());
    let message: String = (message.trim().char_indices())
        .take_while(|(at, c)| at + c.len_utf8() <= room)
        .map(|(_, c)| c)
        .collect();
    format!("{prefix}{message}")
}

/// The title a model wrote, as Codex's `parse_thread_title` reads it: a
/// JSON object's `title`, its quotes, runs of space and ending `.?!`
/// dropped, cut to [`WRITTEN`] characters; none where nothing is left.
pub fn parse(reply: &str) -> Option<String> {
    if !reply.trim_start().starts_with('{') {
        return None;
    }
    let reply: serde_json::Value = serde_json::from_str(reply).ok()?;
    let title = (reply.as_object())
        .filter(|reply| reply.len() == 1)
        .and_then(|reply| reply.get("title")?.as_str())?;
    let title = (title.trim())
        .trim_matches(|c| matches!(c, '"' | '\'' | '`' | '“' | '”' | '‘' | '’'))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let title = title.trim_end_matches(['.', '?', '!']).trim_end();
    (!title.is_empty()).then(|| title.chars().take(WRITTEN).collect())
}

fn parse_reply(reply: &str, _: &[&str]) -> Result<agent_client::judge::Answers, String> {
    let title = parse(reply).ok_or("the title's reply holds no title")?;
    Ok([("title".to_owned(), serde_json::Value::String(title))]
        .into_iter()
        .collect())
}

/// The model that writes a title for an agent running `model`, and its
/// effort, as Codex picks: its small model at low effort where the agent
/// runs on ChatGPT and `listed` (the person's list of models) has it,
/// else the agent's own at its own effort.
pub fn writer(model: &str, listed: impl Fn(&str) -> bool) -> (String, Option<&'static str>) {
    match model.starts_with("chatgpt/") && listed(SMALL) {
        true => (SMALL.to_owned(), Some("low")),
        false => (model.to_owned(), None),
    }
}

/// The base bot whose forks write titles with `model`: one of Sonny's own,
/// one a model, its name in the bytes a bot's name may hold.
fn base_name(model: &str) -> String {
    let room = 128 - crate::project::INTERNAL.len() - agent_client::judge::FORK_SUFFIX;
    let model: String = (model.bytes())
        .map(|b| match b.is_ascii_alphanumeric() || b"-_.".contains(&b) {
            true => b as char,
            false => '_',
        })
        .collect();
    let name = format!("title.{model}");
    crate::project::internal(&name[..name.len().min(room)])
}

/// Have a model write the title of `bot`, the agent of id `id` running
/// `model`, whose first message is `message`, and note it in place of the
/// one its message made. A title that could not be written leaves that
/// one, as Codex keeps its fallback.
pub async fn generate(
    client: &agent_client::Client,
    store: &Path,
    bot: &str,
    id: i64,
    message: &str,
    model: &str,
) -> Result<String, String> {
    let listed = tokio::task::spawn_blocking(|| {
        (agent_client::models::path())
            .and_then(|path| agent_client::models::read(&path).ok())
            .is_some_and(|models| models.iter().any(|listed| listed.id == SMALL))
    })
    .await
    .unwrap_or(false);
    let (model, effort) = writer(model, |_| listed);
    let base = agent_client::judge::Base::make(
        client,
        &base_name(&model),
        &model,
        effort,
        INSTRUCTIONS,
        &[],
    )
    .await
    .map_err(|error| error.to_string())?;
    let deadline = std::time::Instant::now() + DEADLINE;
    let (answers, _) = (base.ask_with(client, &prompt(message), &[], &REPLY, None, deadline))
        .await
        .map_err(|failed| failed.why)?;
    let title = answers["title"].as_str().unwrap_or_default().to_owned();
    let store = store.to_owned();
    let (noted_bot, noted) = (bot.to_owned(), title.clone());
    tokio::task::spawn_blocking(move || write(&folder(&store), &written(&noted_bot), id, &noted))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())?;
    Ok(title)
}

/// The verb of `sonny title`, which writes one agent's title.
pub const VERB: &str = "title";

/// Have a process of its own write the title of `bot`, the agent of id
/// `id` just started with `message`, with the daemon `env` names (as
/// `AGENT_STORE` and `AGENT_SOCKET`): it outlives the call or the window
/// that started the agent, takes a turn of the writer's, and is gone. A
/// title not written leaves the one the message made.
pub fn spawn(env: &[(String, String)], bot: &str, id: i64, message: &str) {
    use std::os::unix::process::CommandExt;
    let Ok(me) = std::env::current_exe() else {
        return;
    };
    // Only what the request holds, so no argument is ever too long.
    let message: String = (message.trim().char_indices())
        .take_while(|(at, c)| at + c.len_utf8() <= PROMPT_BYTES)
        .map(|(_, c)| c)
        .collect();
    let _ = std::process::Command::new(me)
        .args([VERB, bot, &id.to_string(), &message])
        .envs(env.iter().map(|(key, value)| (key, value)))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        // A group of its own: a call's process group stopping takes it not.
        .process_group(0)
        .spawn();
}

/// `sonny title BOT ID MESSAGE`: write the title of `BOT`, the agent of
/// id `ID` whose first message is `MESSAGE`, on its own model as [`writer`]
/// picks, and note it. An agent of that name with another id is left.
pub fn cli(args: &[String]) -> Result<(), String> {
    let [bot, id, message] = args else {
        return Err("usage: sonny title BOT ID MESSAGE".into());
    };
    let id: i64 = id.parse().map_err(|_| format!("{id} is no agent's id"))?;
    let (store, socket) = crate::daemon_paths(None, None)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    runtime.block_on(async {
        let (client, _events) = agent_client::Client::connect(&socket)
            .await
            .map_err(|error| error.to_string())?;
        let record = (client.request("inspect", serde_json::json!({"bot": bot})))
            .await
            .map_err(|error| error.to_string())?;
        if record["bot_id"].as_i64() != Some(id) {
            return Err(format!("{bot} is another agent now"));
        }
        let model = format!(
            "{}/{}",
            record["provider"].as_str().unwrap_or_default(),
            record["model"].as_str().unwrap_or_default()
        );
        let written = generate(&client, &store, bot, id, message, &model).await;
        client.close().await;
        written.map(|_| ())
    })
}

/// The title noted for `bot`, the agent of id `id`, read from its file.
pub fn noted(store: &Path, bot: &str, id: i64) -> Option<String> {
    let dir = folder(store);
    noted_in(&dir, &written(bot), id).or_else(|| noted_in(&dir, bot, id))
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

/// The title a project's file gives it (`project::read`'s `title`), cut
/// at a word to [`WRITTEN`] characters, as long as a title a model writes.
pub fn of_project(info: &serde_json::Value) -> Option<String> {
    (info["title"].as_str())
        .map(|title| title.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|title| !title.is_empty())
        .map(|title| cut_chars(&title, WRITTEN))
}

/// `title` cut at a word to `most` characters, with `…`.
fn cut_chars(title: &str, most: usize) -> String {
    if title.chars().count() <= most {
        return title.to_owned();
    }
    let room: String = title.chars().take(most - 1).collect();
    // Cut at the last word that ends within the room.
    let whole = title.chars().nth(most - 1) == Some(' ');
    let kept = match room.rfind(' ') {
        Some(at) if at > 0 && !whole => &room[..at],
        _ => &room,
    };
    format!("{}…", kept.trim_end_matches([',', ';', ':', '·', '-', ' ']))
}

/// Whether `bot` is a project's lead, `shop.lead`, whose title is its
/// project's: not another lead of its folder, which the person started
/// with a message, as any other agent.
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
    fn an_agent_shows_its_first_message_on_one_line_as_codex_does() {
        assert_eq!(
            of_message("\n  Tally the invoices  \nby month"),
            "Tally the invoices by month"
        );
        assert_eq!(of_message(""), "");
        // Read as Markdown: a heading's marks and a list's go.
        assert_eq!(
            of_message("# Context\n\n- fix `halves`\n- add tests"),
            "Context fix halves add tests"
        );
        let long = "Round every half cent to the nearest even cent in the ledger and the invoices";
        let title = of_message(long);
        assert_eq!(
            title,
            "Round every half cent to the nearest even cent in the ledge…"
        );
        assert_eq!(title.chars().count(), MOST);
    }

    #[test]
    fn a_title_is_asked_for_in_codex_s_words_within_its_bytes() {
        assert_eq!(
            prompt("  Fix the login form  \n"),
            format!("{INSTRUCTIONS}\n\nUser prompt:\nFix the login form")
        );
        assert!(INSTRUCTIONS.contains(&format!("at most {WRITTEN} characters")));
        assert!(SCHEMA.contains(&format!("\"maxLength\":{WRITTEN}")));
        // A long message is cut at a character, the request within its bytes.
        let asked = prompt(&"é".repeat(PROMPT_BYTES));
        assert!(asked.len() <= PROMPT_BYTES && asked.len() > PROMPT_BYTES - 2);
        assert!(asked.ends_with('é'));
    }

    #[test]
    fn a_written_title_is_read_as_codex_reads_it() {
        assert_eq!(
            parse(r#"{"title":"  \"Fix   login form.\" "}"#).as_deref(),
            Some("Fix login form")
        );
        assert_eq!(parse(r#"{"title":"Why?!"}"#).as_deref(), Some("Why"));
        let long = format!(r#"{{"title":"{}"}}"#, "a".repeat(50));
        assert_eq!(parse(&long).map(|t| t.chars().count()), Some(WRITTEN));
        for none in [
            "Fix login",
            r#"{"title":"   "}"#,
            r#"{"title":"..."}"#,
            r#"{"title":"A","more":1}"#,
            r#"{"name":"A"}"#,
        ] {
            assert_eq!(parse(none), None, "{none}");
        }
    }

    #[test]
    fn codex_s_small_model_writes_titles_on_chatgpt_where_it_is_listed() {
        assert_eq!(
            writer("chatgpt/gpt-6.1-sol", |model| model == SMALL),
            (SMALL.to_owned(), Some("low"))
        );
        // Not listed, or another provider: the agent's own model.
        assert_eq!(
            writer("chatgpt/gpt-6.1-sol", |_| false),
            ("chatgpt/gpt-6.1-sol".to_owned(), None)
        );
        assert_eq!(
            writer("anthropic/claude-opus-5-5", |_| true),
            ("anthropic/claude-opus-5-5".to_owned(), None)
        );
        // Each model's writer is a base of its own, under Sonny's mark.
        let base = base_name("work/us.anthropic.claude-opus-5-5:0");
        assert_eq!(base, ".title.work_us.anthropic.claude-opus-5-5_0");
        assert!(crate::project::is_internal(&base));
        let long = base_name(&"m".repeat(200));
        assert!(long.len() + agent_client::judge::FORK_SUFFIX <= 128);
    }

    #[test]
    fn a_written_title_wins_over_the_message_s_in_every_window() {
        let dir = std::env::temp_dir().join(format!("sonny-written-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = dir.join("state.sqlite");
        note(
            &store,
            "calc.fix",
            2,
            "Fix the sums in the ledger so every column adds up",
        )
        .unwrap();
        let first = Noted::read(&store);
        assert_eq!(
            first.of("calc.fix", Some(2)).as_deref(),
            Some("Fix the sums in the ledger so every column adds up")
        );
        // Another window writes the model's: a file of its own, which
        // this one sees as the folder changes.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(folder(&store).join("calc.fix@"), "2\nFix ledger sums\n").unwrap();
        assert_eq!(
            Noted::read(&store).of("calc.fix", Some(2)).as_deref(),
            Some("Fix ledger sums")
        );
        assert_eq!(
            noted(&store, "calc.fix", 2).as_deref(),
            Some("Fix ledger sums")
        );
        // Another agent given the name later takes neither.
        assert_eq!(Noted::read(&store).of("calc.fix", Some(9)), None);
        let _ = std::fs::remove_dir_all(&dir);
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
        // Never its first message, noted before projects named leads.
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
        // Another lead the person started with /new keeps its own.
        note(&store, "tally.lead.notes", 3, "Draft the release notes").unwrap();
        assert_eq!(
            titles(&bot("tally.lead.notes", 3)).as_deref(),
            Some("Draft the release notes")
        );
        // A long title in the file is cut at a word, as long as a written one.
        std::fs::write(
            dir.join(agent_client::project::FILE),
            "name = \"tally\"\ntitle = \"Tally   the invoices against the bank statements\"\n",
        )
        .unwrap();
        assert_eq!(
            titles(&bot("tally.lead", 1)).as_deref(),
            Some("Tally the invoices against the bank…")
        );
        // A project file with none: the lead has none, so its name shows.
        std::fs::write(dir.join(agent_client::project::FILE), "name = \"tally\"\n").unwrap();
        assert_eq!(titles(&bot("tally.lead", 1)), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
