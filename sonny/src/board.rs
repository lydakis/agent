//! Boards: work an agent opens in the commons for others to take. A board
//! is a path whose first segment is a project, as `shop` or
//! `shop/typed-errors`, made by its first post and never moved. A board
//! holds plain threads beside work: work is a commons post whose thread
//! carries where it stands. Each step of work is a post Sonny marks with
//! `do`, so its thread keeps every step, and where work stands is what its
//! posts say, read from them each time, never kept beside them. Anyone may
//! read work and post in its thread; only the agents of the board's
//! project open, take or change it. The commons tool carries it all: a
//! `post` with a board, a `reply` with `do` or work's fields.
use crate::commons;
use crate::lookout::subs::Sub;
use serde_json::{Value, json};
use std::path::Path;

/// What an agent does to work, as a reply's `do`: open it, take it, drop
/// it back, close it done or failed, or pause it until it is opened again.
pub const VERBS: [&str; 6] = ["open", "take", "drop", "done", "fail", "pause"];

/// The most of work's title, as of a task's: its verb first.
pub const TITLE_CHARS: usize = 30;

/// Work's fields that its steps change or clear after it is made.
pub const CHANGING: [&str; 6] = ["for", "labels", "needs", "holder", "holder_id", "result"];

/// Names a label never takes: what Sonny writes of work.
pub const PROTECTED: [&str; 7] = ["state", "holder", "board", "by", "project", "at", "do"];

/// How `for` names a profile rather than an agent.
pub const PROFILE: &str = "profile:";

/// Whether agents have boards: `[sonny] boards`, on by default, and only
/// with the commons, where their work lives. Read once, as the commons'
/// switch is.
pub fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        commons::on() && agent_client::config::load().map_or(true, |config| config.sonny.boards)
    })
}

/// The project a board is the work of: its first segment.
pub fn project_of(board: &str) -> &str {
    board.split('/').next().unwrap_or(board)
}

/// A board is its project's name, then segments of a-z 0-9 - . _, each
/// 1-64 of them, joined by /.
pub fn valid_board(board: &str) -> Result<(), String> {
    let segment = |s: &str| {
        (1..=64).contains(&s.len())
            && !s.starts_with('.')
            && (s.bytes()).all(|b| {
                b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.' || b == b'_'
            })
    };
    let mut segments = board.split('/');
    let project = segments.next().unwrap_or_default();
    match agent_client::project::valid_name(project) && segments.all(segment) {
        true => Ok(()),
        false => Err(format!(
            "work_invalid: a board is its project's name, then /-separated parts of a-z 0-9 - . _, \
             as shop/typed-errors; not {board:?}"
        )),
    }
}

/// What a board's follow hears, as the commons source names each step on
/// a board in its `event`: work opening (new, open again, or its needs
/// met), taken, dropped, settling, a plain thread starting there, the
/// board's last work settling, held work going quiet, and work paused.
pub const EVENTS: [&str; 10] = [
    "opened", "taken", "dropped", "done", "failed", "canceled", "posted", "emptied", "quiet",
    "paused",
];

/// A pattern every event of the commons matches, as a steer: each post,
/// and a follow's word that it went quiet, has text.
pub fn every() -> Value {
    json!({"text": [{"exists": true}]})
}

/// What follows `board` for an agent: each of its [`EVENTS`] as it comes,
/// cutting into a busy turn, so a worker that finished one piece hears
/// the next open and a lead hears the board as it moves. A task's agents
/// on the board follow it, the lead that started the task, and any agent
/// that opens or takes work there. It ends with its agent.
pub fn follow(board: &str, owner: Option<i64>) -> Sub {
    Sub {
        name: Sub::bounded(format!("board.{}", board.replace('/', "."))),
        tasks_of: Vec::new(),
        agents: Vec::new(),
        sources: vec![agent_client::config::COMMONS_SOURCE.to_owned()],
        filter: Some(json!({"board": [board], "event": EVENTS})),
        digest: None,
        steer: Some(every()),
        owner,
        until: None,
        runs: None,
        made: None,
        outlives_close: false,
        quiet: None,
        about: Some(format!("you follow the board {board}")),
    }
}

/// Why a task's agent follows the board its task works from, which marks
/// that follow as its task's board among others it may follow.
const WORKS_FROM: &str = "you work from the board ";

/// The follow of the board a task works from, by an agent of it known as
/// `owner`: [`follow`], marked as its task's.
pub fn works_from(board: &str, owner: Option<i64>) -> Sub {
    Sub {
        about: Some(format!("{WORKS_FROM}{board}")),
        ..follow(board, owner)
    }
}

/// Where a call's answer names the work that opened or was given whom it
/// is for, which its tool host asks them to take: taken out of the answer
/// before the agent sees it.
pub const ASK_FOR: &str = "ask_for";

/// Why a thread on a board reaches an agent that holds its work.
pub const HOLDS: &str =
    "you hold this work: once it goes quiet, say where it stands, finish it or drop it";
/// Why a thread on a board reaches an agent its work is for.
pub const ASKED: &str = "you're asked to take this work";
/// Why a thread on a board reaches an agent that posted in it.
pub const POSTED: &str = "you posted here";

/// What follows work's thread for an agent, for `about`'s reason: each
/// post as it comes, cutting into a busy turn, and, with `quiet`, a word
/// once nothing is posted there for that long. Unlike a plain thread an
/// agent posts in, it ends when the agent closes: work is what it was
/// there for.
pub fn post_follow(thread: &str, owner: Option<i64>, about: &str, quiet: Option<String>) -> Sub {
    let mut sub = Sub::commons(thread, owner);
    sub.steer = Some(every());
    sub.about = Some(about.to_owned());
    sub.quiet = quiet;
    sub.outlives_close = false;
    sub
}

/// `caller`, known as `id`, follows `board`, unless it already works
/// from it: its task's board stays marked as its task's.
pub fn follow_kept(store: &Path, caller: &str, board: &str, id: Option<i64>) {
    let named = follow(board, id);
    let task = (crate::lookout::subs::read(store, caller).iter())
        .any(|sub| sub.name == named.name && sub.about.as_deref().is_some_and(works));
    if !task {
        let _ = crate::lookout::subs::put(store, caller, named);
    }
}

/// What a call to the commons tool that `said` this follows for the
/// agents it touches, in `store`: the board for `caller`, known as `id`,
/// when it opened or took work there, and for an agent another dropped
/// work from, its thread as one that posted there. The caller's own
/// follow of the thread, as [`commons::subscribed`] takes it: as its
/// holder, one the work is for, or one that posted; none for a thread on
/// no board.
pub fn follows(
    store: &Path,
    caller: &str,
    id: Option<i64>,
    arguments: &Value,
    said: &Value,
) -> Option<Sub> {
    let thread = said["thread"].as_str()?;
    let work = &said["work"];
    let board = work["board"].as_str().or(said["board"].as_str())?;
    let step = arguments["reply"]["do"].as_str();
    if work.is_object()
        && (arguments["post"]["work"] == true || matches!(step, Some("open" | "take")))
    {
        follow_kept(store, caller, board, id);
    }
    if let Some(name) = said["dropped_from"]["name"].as_str() {
        let held_by = said["dropped_from"]["id"].as_i64();
        let _ = crate::lookout::subs::put(store, name, post_follow(thread, held_by, POSTED, None));
    }
    let holds = work["state"] == "taken" && work["holder"] == caller;
    let asked = matches!(work["state"].as_str(), Some("open"))
        && (work["for"].as_array().into_iter().flatten()).any(|named| named == caller);
    let mut follow = match (holds, asked) {
        (true, _) => post_follow(thread, id, HOLDS, hold_quiet()),
        (false, true) => post_follow(thread, id, ASKED, None),
        _ => post_follow(thread, id, POSTED, None),
    };
    // A plain thread on a board lasts while its agent is closed, as any
    // commons thread it posts in; work's thread ends with it.
    follow.outlives_close = !work.is_object();
    Some(follow)
}

/// The agents a post or a reply of the commons tool names as `@NAME`, in
/// the order it names them, once each, `caller` aside: a name with a dot
/// in it, as agents' are, at a word's start, so an email, `@types/node`
/// and the like stay text. Whether each is a live agent is the caller's
/// to ask.
pub fn mentioned(arguments: &Value, caller: &str) -> Vec<String> {
    let text = (arguments["post"]["text"].as_str())
        .or(arguments["reply"]["text"].as_str())
        .unwrap_or_default();
    let name_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-');
    let mut named: Vec<String> = Vec::new();
    let mut before: Option<char> = None;
    for (at, c) in text.char_indices() {
        let starts = c == '@' && !before.is_some_and(|b| name_char(b) || matches!(b, '@' | '/'));
        before = Some(c);
        if !starts {
            continue;
        }
        let rest = &text[at + 1..];
        let end = rest.find(|c: char| !name_char(c)).unwrap_or(rest.len());
        let name = rest[..end].trim_end_matches(['.', '-', '_']);
        let follows = rest[end..].chars().next();
        if name.contains('.')
            && name.len() <= 128
            && follows != Some('/')
            && name != caller
            && !named.iter().any(|n| n == name)
        {
            named.push(name.to_owned());
        }
    }
    named
}

/// How long work taken may go without a post before its holder is asked
/// whether it is still on it, as a follow's `quiet`: `[sonny] hold_quiet`
/// minutes, 15 by default; none for 0.
pub fn hold_quiet() -> Option<String> {
    let minutes = crate::kept::config().map_or(15, |config| config.sonny.hold_quiet);
    (minutes > 0).then(|| format!("{minutes}m"))
}

/// What the post at `n` of a thread on a board did there, as the board's
/// follow hears it: one of [`EVENTS`], or `waiting` (work posted that
/// waits on its needs), `set` (fields changed) or `reply` (talk).
pub fn event_of(posts: &[Value], n: usize, thread: &str) -> &'static str {
    let post = &posts[n];
    let opens = (post["opened"].as_array().into_iter().flatten()).any(|t| t == thread);
    match post["do"].as_str() {
        None if n == 0 => "posted",
        None => "reply",
        Some("take") => "taken",
        Some("drop") => "dropped",
        Some("done") => "done",
        Some("pause") => "paused",
        Some("fail") => match standing(&posts[..=n]).is_some_and(|it| it["state"] == "canceled") {
            true => "canceled",
            false => "failed",
        },
        Some("open" | "set") if opens => "opened",
        Some("open") => "waiting",
        Some(_) => "set",
    }
}

/// The board the agent `agent` works from, as its board follow in
/// `store` names it: none when it follows none.
pub fn followed(store: &std::path::Path, agent: &str) -> Option<String> {
    let boards: Vec<Sub> = (crate::lookout::subs::read(store, agent).into_iter())
        .filter(|sub| sub.name.starts_with("board."))
        .collect();
    // Its task's board, before boards it follows for work it took.
    let task = (boards.iter()).find(|sub| sub.about.as_deref().is_some_and(works));
    (task.into_iter().chain(&boards))
        .find_map(|sub| sub.filter.as_ref()?["board"][0].as_str().map(str::to_owned))
}

/// Whether `about` says a follow is of its task's board.
fn works(about: &str) -> bool {
    about.starts_with(WORKS_FROM)
}

/// Work's title: one line of at most [`TITLE_CHARS`], as a task's.
pub fn valid_title(title: &str) -> Result<(), String> {
    let fine = !title.trim().is_empty()
        && !title.contains('\n')
        && title.trim().chars().count() <= TITLE_CHARS;
    match fine {
        true => Ok(()),
        false => Err(format!(
            "work_invalid: work's title is one line of at most {TITLE_CHARS} characters, its \
             verb first, as Fix the login redirect; not {title:?}"
        )),
    }
}

/// Where the work a thread's posts make stands, from its marked posts in
/// order: None for a thread no `open` made work. Its `state` is open,
/// paused, taken, done, failed or canceled; whether an open one waits on its
/// needs is [`waiting`]'s, which knows the other work.
///
/// `{board, state, by, by_id?, at, labels, needs, for?, title?, holder?, holder_id?, result?,
/// branch?, commit?, uncommitted?}`: `by` made it, as the daemon knew it by `by_id`, `at` is its
/// last step's time, `holder` holds taken work and did done or failed work, `result` is a closing
/// post's text, `branch`, `commit` and `uncommitted` the checkout a done post recorded
/// ([`MADE`]), and `title` is the short one a reply opening a thread already there gave it.
pub fn standing(posts: &[Value]) -> Option<Value> {
    let mut item: Option<Value> = None;
    for post in posts {
        let Some(action) = post["do"].as_str() else {
            continue;
        };
        let made = match (&item, action) {
            (None, "open") => {
                let mut made = json!({"board": post["board"], "state": "open", "by": post["by"],
                    "at": post["at"], "labels": {}, "needs": []});
                if post["by_id"].is_i64() {
                    made["by_id"] = post["by_id"].clone();
                }
                // A reply opening a thread gives its short title.
                if post["title"].is_string() && !std::ptr::eq(post, &posts[0]) {
                    made["title"] = post["title"].clone();
                }
                item = Some(made);
                true
            }
            (None, _) => continue,
            (Some(_), _) => false,
        };
        let Some(it) = item.as_mut().and_then(Value::as_object_mut) else {
            continue;
        };
        let unheld = matches!(
            it.get("state").and_then(Value::as_str),
            Some("open" | "paused")
        );
        let mut set = |key: &str, value: Value| _ = it.insert(key.to_owned(), value);
        match action {
            "open" if !made => {
                set("state", json!("open"));
                set("holder", Value::Null);
                set("holder_id", Value::Null);
                set("result", Value::Null);
            }
            "take" => {
                set("state", json!("taken"));
                set("holder", post["by"].clone());
                set("holder_id", post["by_id"].clone());
                set("result", Value::Null);
            }
            "drop" => {
                set("state", json!("open"));
                set("holder", Value::Null);
                set("holder_id", Value::Null);
            }
            "done" => {
                set("state", json!("done"));
                set("result", post["text"].clone());
            }
            "pause" => set("state", json!("paused")),
            // Its maker's fail while nobody holds it takes it back.
            "fail" => {
                set("state", json!(if unheld { "canceled" } else { "failed" }));
                set("result", post["text"].clone());
            }
            _ => {}
        }
        // What a done post recorded of its author's checkout.
        if action == "done" {
            for key in MADE.into_iter().filter(|key| !post[*key].is_null()) {
                it.insert(key.to_owned(), post[key].clone());
            }
        }
        // Fields any marked post carries change the work as of it.
        if let Some(labels) = post["labels"].as_object() {
            let held = it.entry("labels").or_insert_with(|| json!({}));
            for (key, value) in labels {
                match value {
                    Value::Null => _ = held.as_object_mut().map(|l| l.remove(key)),
                    value => held[key] = value.clone(),
                }
            }
        }
        if post["needs"].is_array() {
            it.insert("needs".into(), post["needs"].clone());
        }
        if let Some(named) = post["for"].as_array() {
            it.insert(
                "for".into(),
                match named.is_empty() {
                    true => Value::Null,
                    false => json!(named),
                },
            );
        }
        it.insert("at".into(), post["at"].clone());
        it.retain(|_, value| !value.is_null());
    }
    item
}

/// For each of a thread's posts, whether it is a `fail` that canceled
/// its work: its maker's, while no one held it, as [`standing`] reads it.
/// One pass, so a long thread's page asks no more than its posts.
pub fn cancels(posts: &[Value]) -> Vec<bool> {
    let mut unheld: Option<bool> = None;
    (posts.iter())
        .map(|post| {
            let action = post["do"].as_str();
            let canceled = action == Some("fail") && unheld == Some(true);
            unheld = match (unheld, action) {
                (_, Some("open" | "drop")) => Some(true),
                (Some(_), Some("take" | "done" | "fail")) => Some(false),
                (now, _) => now,
            };
            canceled
        })
        .collect()
}

/// Whether `item` waits: open, with a need that `done` says is not done.
/// The listing and the board's own page alike ask this of work, with
/// what they know of the rest.
pub fn waiting(item: &Value, done: &dyn Fn(&str) -> bool) -> bool {
    item["state"] == "open"
        && (item["needs"].as_array().into_iter().flatten())
            .any(|need| !need.as_str().is_some_and(done))
}

/// Work as every answer and listing shows it: open work with a need not
/// done says `waiting`, so what an agent is told of work is the same
/// wherever it is told.
pub fn shown(dir: &Path, mut item: Value) -> Value {
    if waiting(&item, &|need: &str| need_done(dir, need)) {
        item["state"] = json!("waiting");
    }
    item
}

/// Who calls the tool: the agent, the daemon's id for it, whom what it is
/// shown counts for (as `commons::who` names it), the model it writes
/// with, the project it is named in, the profile it was made as, whether
/// it is a lead, and the folder it works in, whose checkout its `done`
/// records.
pub struct Caller<'a> {
    pub name: &'a str,
    pub id: Option<i64>,
    pub reader: &'a str,
    pub model: Option<&'a str>,
    pub project: Option<&'a str>,
    pub profile: Option<&'a str>,
    pub lead: bool,
    pub folder: Option<&'a Path>,
}

/// What a `done` post records of its author's checkout, as git there
/// says: its `branch` (none on a detached head), its `commit`, and
/// `uncommitted` when changes there are not in it.
pub const MADE: [&str; 3] = ["branch", "commit", "uncommitted"];

/// `folder`'s checkout as a `done` post records it ([`MADE`]); nothing
/// for a folder that is no git checkout with a commit.
pub fn checkout(folder: &Path) -> serde_json::Map<String, Value> {
    let git = |args: &[&str]| crate::task::git(folder, args).ok();
    let mut made = serde_json::Map::new();
    let Some(commit) = git(&["rev-parse", "--verify", "--quiet", "HEAD"]) else {
        return made;
    };
    if let Some(branch) = git(&["symbolic-ref", "--short", "--quiet", "HEAD"]) {
        made.insert("branch".into(), json!(branch));
    }
    made.insert("commit".into(), json!(commit));
    // Reading its files can run what the repository's config names (a
    // clean filter), so as an agent's git runs, as a task's removal reads
    // it; untracked files count whatever the config shows.
    let status = [
        "--no-optional-locks",
        "status",
        "--porcelain",
        "--untracked-files=all",
        "--ignore-submodules=none",
    ];
    if crate::task::confined(folder, &status, &[]).is_ok_and(|changed| !changed.is_empty()) {
        made.insert("uncommitted".into(), json!(true));
    }
    made
}

/// Whether the commons tool's `post` puts its thread on a board.
pub fn posts_work(given: &Value) -> bool {
    ["board", "work", "needs", "labels", "for"]
        .iter()
        .any(|key| !given[*key].is_null())
}

/// Whether the commons tool's `reply` does something to work.
pub fn replies_work(given: &Value) -> bool {
    ["do", "board", "title", "needs", "labels", "for"]
        .iter()
        .any(|key| !given[*key].is_null())
}

/// Each work's summary in the commons, by thread, read under the lock the
/// caller holds.
fn items(dir: &Path) -> Vec<(String, Value)> {
    (commons::summaries(dir).into_iter())
        .filter(|(_, thread)| thread["work"].is_object())
        .collect()
}

/// Work's title, as its board shows it: the short one it was opened
/// with, else its thread's.
fn title_of(thread: &Value) -> &str {
    (thread["work"]["title"].as_str())
        .or(thread["title"].as_str())
        .unwrap_or_default()
}

/// Work still to be done on `board` titled `title` but for case, other
/// than `but`: a second would be the same work twice. Paused work is
/// still to be done.
fn same_title<'a>(
    items: &'a [(String, Value)],
    board: &str,
    title: &str,
    but: &str,
) -> Option<&'a str> {
    let title = title.trim().to_lowercase();
    (items.iter())
        .find(|(name, thread)| {
            name != but
                && thread["work"]["board"] == board
                && matches!(
                    thread["work"]["state"].as_str(),
                    Some("open" | "taken" | "paused")
                )
                && title_of(thread).trim().to_lowercase() == title
        })
        .map(|(name, _)| name.as_str())
}

/// The labels given, each a string, none of them a name Sonny writes; with
/// `remove`, a null removes one.
fn labels(given: &Value, remove: bool) -> Result<serde_json::Map<String, Value>, String> {
    let Some(given) = given.as_object() else {
        return Err("work_invalid: labels are an object of names to strings".into());
    };
    for (key, value) in given {
        if PROTECTED.contains(&key.as_str()) || key.is_empty() {
            return Err(format!(
                "work_invalid: {key:?} is no label's: Sonny writes {}",
                PROTECTED.join(", ")
            ));
        }
        if !(value.is_string() || (remove && value.is_null())) {
            return Err(format!(
                "work_invalid: label {key} is a string{}",
                if remove { ", or null to remove it" } else { "" }
            ));
        }
    }
    Ok(given.clone())
}

/// The threads `needs` names, each a thread's name, once each.
fn needs(given: &Value) -> Result<Vec<String>, String> {
    let refused = || "work_invalid: needs are the names of work on the board".to_owned();
    let mut needs: Vec<String> = Vec::new();
    for need in given.as_array().ok_or_else(refused)? {
        let need = need.as_str().map(str::trim).ok_or_else(refused)?;
        commons::valid_name(need)?;
        if !needs.iter().any(|n| n == need) {
            needs.push(need.to_owned());
        }
    }
    Ok(needs)
}

/// Whom `for` reserves work for: agents by name and profiles as
/// `profile:NAME`; empty frees it.
fn reserved(given: &Value) -> Result<Vec<String>, String> {
    let refused = || {
        format!(
            "work_invalid: for is a list of agents' names and {PROFILE}NAME for a profile's agents"
        )
    };
    let mut named: Vec<String> = Vec::new();
    for one in given.as_array().ok_or_else(refused)? {
        let one = one.as_str().map(str::trim).ok_or_else(refused)?;
        let fine = match one.strip_prefix(PROFILE) {
            Some(profile) => !profile.is_empty() && !profile.contains(char::is_whitespace),
            None => !one.is_empty() && !one.contains(char::is_whitespace),
        };
        if !fine {
            return Err(refused());
        }
        if !named.iter().any(|n| n == one) {
            named.push(one.to_owned());
        }
    }
    Ok(named)
}

/// Whether `caller` is one `for` names: by name, or by the profile it
/// was made as. Work for no one is anyone's.
pub fn is_for(item: &Value, name: &str, profile: Option<&str>) -> bool {
    match item["for"].as_array().filter(|named| !named.is_empty()) {
        None => true,
        Some(named) => named.iter().filter_map(Value::as_str).any(|one| {
            one == name || (one.strip_prefix(PROFILE)).is_some_and(|p| Some(p) == profile)
        }),
    }
}

/// Puts `needs`, `labels` and `for` the given call names onto `post`,
/// checked; `remove` lets a label be removed.
fn fields(post: &mut Value, given: &Value, remove: bool) -> Result<(), String> {
    if !given["needs"].is_null() {
        post["needs"] = json!(needs(&given["needs"])?);
    }
    if !given["labels"].is_null() {
        let labels = labels(&given["labels"], remove)?;
        if !labels.is_empty() {
            post["labels"] = Value::Object(labels);
        }
    }
    if !given["for"].is_null() {
        post["for"] = json!(reserved(&given["for"])?);
    }
    Ok(())
}

/// Whether `needs`, as `thread`'s, lead back to `thread` through the
/// needs of the work in `items`.
fn cycles(items: &[(String, Value)], thread: &str, needs: &[String]) -> bool {
    let mut seen: Vec<String> = Vec::new();
    let mut next: Vec<String> = needs.to_vec();
    while let Some(at) = next.pop() {
        if at == thread {
            return true;
        }
        if seen.contains(&at) {
            continue;
        }
        if let Some((_, it)) = items.iter().find(|(name, _)| *name == at) {
            next.extend(
                (it["work"]["needs"].as_array().into_iter().flatten())
                    .filter_map(Value::as_str)
                    .map(str::to_owned),
            );
        }
        seen.push(at);
    }
    false
}

/// Whether every one of `needs` is work on `board`, and whether all are
/// done; refused naming one that is not.
fn needs_met(items: &[(String, Value)], board: &str, needs: &[String]) -> Result<bool, String> {
    let mut met = true;
    for need in needs {
        let state = (items.iter())
            .find(|(name, it)| name == need && it["work"]["board"] == board)
            .map(|(_, it)| it["work"]["state"].as_str().unwrap_or_default());
        match state {
            Some(state) => met &= state == "done",
            None => {
                return Err(format!(
                    "work_invalid: {need} is no work on {board}; post it first"
                ));
            }
        }
    }
    Ok(met)
}

fn not_ours(board: &str, what: &str) -> String {
    format!(
        "work_not_yours: only {}'s agents {what} on {board}; post in the thread to help",
        project_of(board)
    )
}

/// The commons tool's `post` that puts its thread on a board: a plain
/// thread there, or with `work`, work that opens once its needs are done.
pub fn post(dir: &Path, caller: &Caller, given: &Value) -> Result<Value, String> {
    let text = |key: &str| given[key].as_str().unwrap_or_default();
    let (name, title, board) = (text("name"), text("title"), text("board"));
    commons::valid_name(name)?;
    if board.is_empty() {
        return Err("work_invalid: work goes on a board: give board".into());
    }
    valid_board(board)?;
    let work = given["work"] == true;
    if !work && !(given["needs"].is_null() && given["labels"].is_null() && given["for"].is_null()) {
        return Err("work_invalid: needs, labels and for go with work: true".into());
    }
    let mut post = commons::entry(
        caller.name,
        caller.id,
        caller.model,
        caller.project,
        text("text"),
        given,
    )?;
    post["board"] = json!(board);
    if !work {
        return commons::start(dir, name, title, &post, caller.reader);
    }
    if caller.project != Some(project_of(board)) {
        return Err(not_ours(board, "open work"));
    }
    valid_title(title)?;
    post["do"] = json!("open");
    fields(&mut post, given, false)?;
    let needs: Vec<String> = serde_json::from_value(post["needs"].clone()).unwrap_or_default();
    let _lock = commons::lock(dir)?;
    let items = items(dir);
    if let Some(same) = same_title(&items, board, title, name) {
        return Err(format!(
            "work_exists: {same} on {board} is \"{}\" already; take it, or post there",
            title.trim()
        ));
    }
    let opens = needs_met(&items, board, &needs)?;
    if opens {
        post["opened"] = json!([name]);
    }
    let mut said = commons::start_locked(dir, name, title.trim(), &post, caller.reader)?;
    said["work"] = shown(
        dir,
        standing(std::slice::from_ref(&post)).unwrap_or_default(),
    );
    // Open work kept for some asks them to take it.
    if opens && post["for"].is_array() {
        said[ASK_FOR] = json!([name]);
    }
    Ok(said)
}

/// The commons tool's `reply` that does something to work: one of
/// [`VERBS`] as `do`, fields that change it, or both, `do` checked against
/// the work as it stood before the reply. `gone` says whether a holder, by
/// name and id, is closed or no longer there: taking its work then takes
/// it over.
pub fn reply(
    dir: &Path,
    caller: &Caller,
    given: &Value,
    gone: &dyn Fn(&str, Option<i64>) -> bool,
) -> Result<Value, String> {
    let text = |key: &str| given[key].as_str().unwrap_or_default();
    let thread = text("thread");
    commons::valid_name(thread)?;
    let action = given["do"].as_str();
    if let Some(action) = action
        && !VERBS.contains(&action)
    {
        return Err(format!("work_invalid: do is one of {}", VERBS.join(", ")));
    }
    let mut post = commons::entry(
        caller.name,
        caller.id,
        caller.model,
        caller.project,
        text("text"),
        given,
    )?;
    post["do"] = json!(action.unwrap_or("set"));
    fields(&mut post, given, true)?;
    let changes = ["needs", "labels", "for"]
        .iter()
        .any(|key| !post[*key].is_null());
    if action.is_none() && !changes {
        return Err("work_invalid: a reply to work gives do, or needs, labels or for".into());
    }
    let _lock = commons::lock(dir)?;
    let posts = commons::posts(&dir.join(format!("{thread}.jsonl")))?;
    let current = standing(&posts);
    let ours = |board: &str| caller.project == Some(project_of(board));
    let items = items(dir);
    // A board and a title go with opening a thread that is no work yet.
    if (!given["board"].is_null() || !given["title"].is_null())
        && !(action == Some("open") && current.is_none())
    {
        return Err(
            "work_invalid: board and title go with do open, on a thread that is no work yet".into(),
        );
    }
    let mut opened: Vec<String> = Vec::new();
    let mut answer_needs = false;
    match (action, &current) {
        (Some("open"), None) => {
            let board = text("board");
            if board.is_empty() {
                return Err(format!(
                    "work_invalid: opening {thread} as work names its board"
                ));
            }
            valid_board(board)?;
            if !ours(board) {
                return Err(not_ours(board, "open work"));
            }
            let title = match text("title").trim() {
                "" => posts[0]["title"].as_str().unwrap_or_default(),
                given => given,
            };
            valid_title(title)
                .map_err(|e| format!("{e}: give title, a short one for its board"))?;
            if let Some(same) = same_title(&items, board, title, thread) {
                return Err(format!(
                    "work_exists: {same} on {board} is \"{title}\" already"
                ));
            }
            post["board"] = json!(board);
            if !text("title").trim().is_empty() {
                post["title"] = json!(title.trim());
            }
            let needs: Vec<String> =
                serde_json::from_value(post["needs"].clone()).unwrap_or_default();
            if cycles(&items, thread, &needs) {
                return Err(format!("work_invalid: {thread} would need itself"));
            }
            if needs_met(&items, board, &needs)? {
                opened.push(thread.to_owned());
            }
        }
        (_, None) => {
            return Err(format!(
                "work_not_work: {thread} is no work; reply with do open and its board first"
            ));
        }
        (_, Some(it)) => {
            let board = it["board"].as_str().unwrap_or_default();
            let state = it["state"].as_str().unwrap_or_default();
            let holder = it["holder"].as_str();
            let holder_id = it["holder_id"].as_i64();
            let same_agent = |name: Option<&str>, id: Option<i64>| {
                name == Some(caller.name)
                    && (id.is_none() || caller.id.is_none() || id == caller.id)
            };
            let holds = state == "taken" && same_agent(holder, holder_id);
            // The maker, not another agent later given its name.
            let made_it = same_agent(it["by"].as_str(), it["by_id"].as_i64());
            let leads = caller.lead && ours(board);
            if changes {
                if !ours(board) {
                    return Err(not_ours(board, "change work"));
                }
                if !post["needs"].is_null() {
                    if !matches!(state, "open" | "paused") {
                        return Err(format!(
                            "work_closed: {thread} is {state}; needs change only while it is open or paused"
                        ));
                    }
                    let needs: Vec<String> =
                        serde_json::from_value(post["needs"].clone()).unwrap_or_default();
                    if cycles(&items, thread, &needs) {
                        return Err(format!(
                            "work_invalid: those needs would have {thread} need itself"
                        ));
                    }
                    // Unblocked now, it opens as if its last need were done.
                    let was = (it["needs"].as_array().into_iter().flatten())
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect::<Vec<_>>();
                    let met_before = needs_met(&items, board, &was).unwrap_or(false);
                    if needs_met(&items, board, &needs)?
                        && !met_before
                        && action.is_none()
                        && state == "open"
                    {
                        opened.push(thread.to_owned());
                    }
                }
                if !post["for"].is_null() && !matches!(state, "open" | "taken" | "paused") {
                    return Err(format!("work_closed: {thread} is {state}"));
                }
            }
            match action {
                None => {}
                Some("open") => {
                    if !ours(board) {
                        return Err(not_ours(board, "open work"));
                    }
                    if !matches!(state, "failed" | "canceled" | "paused") {
                        return Err(format!(
                            "work_invalid: {thread} is {state}; open only paused, failed or canceled work"
                        ));
                    }
                    // Who paused it says when it is ready: its maker or the lead.
                    if state == "paused" && !(made_it || leads) {
                        return Err(format!(
                            "work_not_maker: only {thread}'s maker or the project's lead opens it once paused"
                        ));
                    }
                    if let Some(same) = same_title(
                        &items,
                        board,
                        title_of(&json!({
                        "work": it, "title": posts[0]["title"]})),
                        thread,
                    ) {
                        return Err(format!(
                            "work_exists: {same} on {board} has its title already"
                        ));
                    }
                    // Reopened, it opens once what it needs is done, as new
                    // work does: until then it waits.
                    let mut after = it.clone();
                    after["state"] = json!("open");
                    if post["needs"].is_array() {
                        after["needs"] = post["needs"].clone();
                    }
                    if !waiting(&after, &|need: &str| need_done(dir, need)) {
                        opened.push(thread.to_owned());
                    }
                }
                Some("take") => {
                    if !ours(board) {
                        return Err(not_ours(board, "take work"));
                    }
                    // As the work stands after this reply's own fields.
                    let mut after = it.clone();
                    for key in ["needs", "for"] {
                        if post[key].is_array() {
                            after[key] = post[key].clone();
                        }
                    }
                    let it = &after;
                    if !is_for(it, caller.name, caller.profile) {
                        let named: Vec<&str> = (it["for"].as_array().into_iter().flatten())
                            .filter_map(Value::as_str)
                            .collect();
                        return Err(format!(
                            "work_reserved: {thread} is for {}; pick other work",
                            named.join(", ")
                        ));
                    }
                    match (state, holder) {
                        ("open", _) => {
                            let undone: Vec<&str> = (it["needs"].as_array().into_iter())
                                .flatten()
                                .filter_map(Value::as_str)
                                .filter(|need| !need_done(dir, need))
                                .collect();
                            if !undone.is_empty() {
                                return Err(format!(
                                    "work_waiting: {thread} needs {} done first",
                                    undone.join(", ")
                                ));
                            }
                        }
                        ("paused", _) => {
                            return Err(format!(
                                "work_paused: {thread} is paused until its maker or the lead opens it; pick other work"
                            ));
                        }
                        ("taken", Some(_)) if holds => {
                            return Err(format!("work_held: {thread} is yours already"));
                        }
                        ("taken", Some(holder)) if !gone(holder, holder_id) => {
                            return Err(format!(
                                "work_held: {holder} holds {thread}; pick other work"
                            ));
                        }
                        ("taken", Some(holder)) => {
                            post["took_from"] = json!(holder);
                            if let Some(id) = holder_id {
                                post["took_from_id"] = json!(id);
                            }
                        }
                        (state, _) => {
                            return Err(format!("work_closed: {thread} is {state}"));
                        }
                    }
                    answer_needs = true;
                }
                Some("drop") => {
                    // Its holder gives it back; its maker or a lead of the
                    // board's project takes it from a live holder, saying why.
                    if state != "taken" || !(holds || made_it || leads) {
                        return Err(format!(
                            "work_not_holder: only {thread}'s holder{}, its maker or the project's lead drops it",
                            holder.map_or(String::new(), |h| format!(", {h},"))
                        ));
                    }
                    opened.push(thread.to_owned());
                }
                Some("done") => {
                    if !holds {
                        return Err(format!(
                            "work_not_holder: only {thread}'s holder closes it{}",
                            holder.map_or(String::new(), |h| format!(", {h}"))
                        ));
                    }
                    opened = opened_by(dir, board, thread);
                    for (key, value) in caller.folder.map(checkout).into_iter().flatten() {
                        post[key] = value;
                    }
                }
                Some("pause") => {
                    // Work no one holds yet, kept from being taken until
                    // its maker or the lead opens it again.
                    if state != "open" {
                        return Err(format!(
                            "work_invalid: {thread} is {state}; pause only open work, which no one holds"
                        ));
                    }
                    if !(made_it || leads) {
                        return Err(format!(
                            "work_not_maker: only {thread}'s maker or the project's lead pauses it"
                        ));
                    }
                }
                Some("fail") => {
                    let unheld = matches!(state, "open" | "paused");
                    if !(holds || (unheld && made_it)) {
                        return Err(format!(
                            "work_not_holder: only {thread}'s holder fails it, or its maker while nobody holds it{}",
                            holder.map_or(String::new(), |h| format!("; {h} holds it"))
                        ));
                    }
                }
                Some(_) => unreachable!("do is one of VERBS"),
            }
        }
    }
    if !opened.is_empty() {
        post["opened"] = json!(opened);
    }
    // The board's last open, paused or held work settling empties it.
    let settles = matches!(action, Some("done" | "fail"));
    if settles
        && opened.is_empty()
        && let Some(board) = current.as_ref().and_then(|it| it["board"].as_str())
        && !(items.iter()).any(|(name, it)| {
            name != thread
                && it["work"]["board"] == board
                && matches!(
                    it["work"]["state"].as_str(),
                    Some("open" | "taken" | "paused")
                )
        })
    {
        post["emptied"] = json!(board);
    }
    // Work that opens, or open work given whom it is for, asks them, as
    // this reply leaves it: needs it adds keep it waiting.
    let mut ask_for = opened.clone();
    let reserves = (post["for"].as_array()).is_some_and(|named| !named.is_empty());
    let updated = || {
        let mut after = posts.clone();
        after.push(post.clone());
        standing(&after)
    };
    if reserves
        && !ask_for.iter().any(|t| t == thread)
        && updated().is_some_and(|it| {
            it["state"] == "open" && !waiting(&it, &|need: &str| need_done(dir, need))
        })
    {
        ask_for.push(thread.to_owned());
    }
    // A holder another drops it from, or takes it over from once closed,
    // follows it as one that posted there, no longer as its holder.
    let dropped_from = match (action, &current) {
        (Some("drop"), Some(it)) if it["holder"] != caller.name => {
            Some(json!({"name": it["holder"], "id": it["holder_id"]}))
        }
        (Some("take"), _) if post["took_from"].is_string() => {
            Some(json!({"name": post["took_from"], "id": post["took_from_id"]}))
        }
        _ => None,
    };
    // A full thread still takes what moves its work, so talk in it never
    // leaves the work stuck where it stands.
    let moves = action.is_some();
    let mut said = commons::add_locked(dir, thread, &post, caller.reader, !moves)?;
    if said["posted"] != false {
        let mut after = posts;
        after.push(post);
        let work = standing(&after).unwrap_or_default();
        // What the work it needed made, so its taker starts from it.
        if answer_needs {
            let made: Vec<Value> = (work["needs"].as_array().into_iter().flatten())
                .filter_map(Value::as_str)
                .filter_map(|need| {
                    let (_, it) = items.iter().find(|(name, _)| name == need)?;
                    let it = &it["work"];
                    let mut made =
                        json!({"thread": need, "by": it["holder"], "result": it["result"]});
                    for key in MADE.into_iter().filter(|key| !it[*key].is_null()) {
                        made[key] = it[key].clone();
                    }
                    Some(made)
                })
                .collect();
            if !made.is_empty() {
                said["needs_made"] = json!(made);
            }
        }
        said["work"] = shown(dir, work);
        if let Some(from) = dropped_from {
            said["dropped_from"] = from;
        }
        if !ask_for.is_empty() {
            said[ASK_FOR] = json!(ask_for);
        }
    }
    Ok(said)
}

/// Whether the work `need` is done.
fn need_done(dir: &Path, need: &str) -> bool {
    commons::valid_name(need).is_ok()
        && commons::posts(&dir.join(format!("{need}.jsonl")))
            .ok()
            .and_then(|posts| standing(&posts))
            .is_some_and(|item| item["state"] == "done")
}

/// The work on `board` that `thread`, done now, opens: open work that
/// needs it, whose other needs are done.
fn opened_by(dir: &Path, board: &str, thread: &str) -> Vec<String> {
    let items = items(dir);
    let done = |need: &str| {
        need == thread
            || (items.iter()).any(|(name, item)| name == need && item["work"]["state"] == "done")
    };
    (items.iter())
        .filter(|(_, item)| {
            let item = &item["work"];
            item["board"] == board
                && item["state"] == "open"
                && (item["needs"].as_array().into_iter().flatten()).any(|n| n == thread)
                && !waiting(item, &done)
        })
        .map(|(name, _)| name.clone())
        .collect()
}

/// Follows of work's threads end when their agent closes (after 0.10.4).
pub const WORK_FOLLOWS_CLOSE: &str = "board-work-follows-close";

/// Each follow of work's thread beside `store`, as [`post_follow`] wrote
/// it for why its agent was there, ends with its agent from now on, once
/// for the store: work's threads in `home`'s commons say which are work.
/// One of a plain thread on a board still lasts while its agent is closed.
pub fn work_follows_close(store: &Path, home: &Path) {
    let mut record = store.as_os_str().to_owned();
    record.push(".migrated");
    let dir = commons::dir(home);
    let prefix = format!("{}.", agent_client::config::COMMONS_SOURCE);
    crate::upgrade::once(Path::new(&record), WORK_FOLLOWS_CLOSE, || {
        crate::lookout::subs::files_changed(store, |sub| {
            let about = sub["about"].as_str();
            let Some(thread) = sub["name"]
                .as_str()
                .and_then(|name| name.strip_prefix(&prefix))
            else {
                return false;
            };
            let work = sub["outlives_close"] == true
                && [HOLDS, ASKED, POSTED].iter().any(|why| about == Some(why))
                && commons::posts(&dir.join(format!("{thread}.jsonl")))
                    .is_ok_and(|posts| standing(&posts).is_some());
            if work && let Some(sub) = sub.as_object_mut() {
                sub.remove("outlives_close");
            }
            work
        })
    });
}

/// Work's steps take the verbs boards have now, the board tool folded
/// into the commons tool (after 0.10.3).
pub const STEPS_RENAMED: &str = "board-steps-renamed";

/// Every step post in `~/.agents/commons` says it with today's verbs, once
/// for this HOME: `add`, `item` and `reopen` are `open`, `claim` is
/// `take`, `release` is `drop`, `cancel` is `fail` (which its maker's on
/// work nobody holds is), `label` and `assign` are `set`, and a `for`
/// naming one agent or profile is the list naming both as they are named
/// now. The board skill, in the commons skill now, goes. A thread that
/// will not be written is tried again next time.
pub fn steps_renamed(home: &Path) {
    crate::upgrade::once(&home.join(".agent/sonny/migrated"), STEPS_RENAMED, || {
        crate::skills::remove_in(home, "board");
        rename_steps(&commons::dir(home))
    });
}

/// One post with today's verb and `for`; whether it changed.
fn renamed(post: &mut Value) -> bool {
    let verb = match post["do"].as_str() {
        Some("add" | "item" | "reopen") => "open",
        Some("claim") => "take",
        Some("release") => "drop",
        Some("cancel") => "fail",
        Some("label" | "assign") => "set",
        _ => return false,
    };
    post["do"] = json!(verb);
    if let Some(named) = post["for"].as_str().map(str::trim) {
        post["for"] = match named.is_empty() {
            true => json!([]),
            false => json!([named, format!("{PROFILE}{named}")]),
        };
    }
    true
}

fn rename_steps(dir: &Path) -> bool {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => return e.kind() == std::io::ErrorKind::NotFound,
    };
    let Ok(_lock) = commons::lock(dir) else {
        return false;
    };
    let mut done = true;
    for entry in entries.flatten() {
        let path = entry.path();
        if !(path.file_name().and_then(|n| n.to_str()))
            .and_then(|n| n.strip_suffix(".jsonl"))
            .is_some_and(|n| commons::valid_name(n).is_ok())
        {
            continue;
        }
        // A thread that will not read is left as it is, for read to name.
        let Ok(mut posts) = commons::posts(&path) else {
            continue;
        };
        let mut changed = false;
        for post in &mut posts {
            changed |= renamed(post);
        }
        if changed {
            // Its time kept: a search orders threads by it.
            let modified = (entry.metadata()).and_then(|meta| meta.modified()).ok();
            let text: String = posts.iter().map(|post| format!("{post}\n")).collect();
            done &= agent_client::files::replace_mode(&path, text.as_bytes(), 0o600).is_ok();
            if let Some(modified) = modified
                && let Ok(file) = std::fs::File::options().write(true).open(&path)
            {
                let _ = file.set_modified(modified);
            }
        }
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn home(name: &str) -> PathBuf {
        let home = std::env::temp_dir().join(format!("sonny-board-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        home
    }

    /// The commons tool for `name` of `project` made as `profile`, a lead
    /// when its name says so, shown every post of the thread it replies
    /// to first, as one that read it is.
    fn send(
        home: &Path,
        name: &str,
        project: &str,
        profile: Option<&str>,
        gone: bool,
        arguments: Value,
    ) -> Result<Value, String> {
        if let Some(thread) = arguments["reply"]["thread"].as_str()
            && let Ok(posts) = commons::posts(&commons::dir(home).join(format!("{thread}.jsonl")))
        {
            commons::told(home, name, thread, &(0..posts.len()).collect::<Vec<_>>());
        }
        let caller = Caller {
            name,
            id: None,
            reader: name,
            model: None,
            project: Some(project),
            profile,
            lead: name.ends_with(".lead"),
            folder: None,
        };
        commons::call_as(home, &caller, &arguments, &|_, _| gone)
    }

    /// A post opening work `name` on shop/errors, with `extra` fields.
    fn work(name: &str, title: &str, extra: Value) -> Value {
        let mut post = json!({"name": name, "title": title, "text": "x",
            "board": "shop/errors", "work": true});
        for (key, value) in extra.as_object().into_iter().flatten() {
            post[key] = value.clone();
        }
        json!({"post": post})
    }

    fn act(thread: &str, what: &str) -> Value {
        json!({"reply": {"thread": thread, "do": what, "text": "y"}})
    }

    /// A reply changing `thread`'s fields alone.
    fn set(thread: &str, fields: Value) -> Value {
        let mut reply = json!({"thread": thread, "text": "y"});
        for (key, value) in fields.as_object().into_iter().flatten() {
            reply[key] = value.clone();
        }
        json!({"reply": reply})
    }

    /// The threads shop/errors lists as `filter` says.
    fn listed(home: &Path, filter: Value) -> Vec<String> {
        let mut filter = filter;
        filter["board"] = json!(["shop/errors"]);
        let listed = send(home, "x.y", "x", None, false, json!({"filter": filter})).unwrap();
        (listed["threads"].as_array().into_iter().flatten())
            .map(|t| t["thread"].as_str().unwrap().to_owned())
            .collect()
    }

    fn open_names(home: &Path) -> Vec<String> {
        listed(home, json!({"state": ["open"]}))
    }

    #[test]
    fn work_that_opens_or_is_kept_for_someone_asks_them_to_take_it() {
        let home = home("ask-for");
        let lead = |args| send(&home, "shop.lead", "shop", None, false, args);
        lead(work("store", "Type the store errors", json!({}))).unwrap();
        let api = lead(work(
            "api",
            "Type the api errors",
            json!({"needs": ["store"], "for": ["profile:worker"]}),
        ))
        .unwrap();
        assert!(
            api.get(ASK_FOR).is_none(),
            "waiting work asks nobody: {api}"
        );
        let kept = lead(set("store", json!({"for": ["shop.w.1"]}))).unwrap();
        assert_eq!(kept[ASK_FOR], json!(["store"]));
        send(&home, "shop.w.1", "shop", None, false, act("store", "take")).unwrap();
        let done = send(&home, "shop.w.1", "shop", None, false, act("store", "done")).unwrap();
        assert_eq!(done[ASK_FOR], json!(["api"]), "{done}");
        // A reply that names someone and adds an undone need asks nobody.
        lead(work("docs", "Write the error docs", json!({}))).unwrap();
        lead(work("guide", "Write the guide", json!({}))).unwrap();
        let blocked = lead(set(
            "guide",
            json!({"needs": ["docs"], "for": ["shop.w.2"]}),
        ))
        .unwrap();
        assert!(blocked.get(ASK_FOR).is_none(), "{blocked}");
        // Waiting work canceled and opened again still waits: no one asked.
        lead(act("guide", "fail")).unwrap();
        let reopened = lead(act("guide", "open")).unwrap();
        assert_eq!(reopened["work"]["state"], "waiting", "{reopened}");
        assert!(reopened.get(ASK_FOR).is_none(), "{reopened}");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn work_opens_when_its_needs_are_done_and_its_steps_are_posts() {
        let home = home("needs");
        let lead = |args| send(&home, "shop.lead", "shop", None, false, args);
        lead(work("store", "Type the store errors", json!({}))).unwrap();
        let api = lead(work("api", "Type the api errors", json!({}))).unwrap();
        assert_eq!(api["work"]["state"], "open");
        let made = lead(work(
            "enum",
            "Remove the old error enum",
            json!({"needs": ["store", "api"]}),
        ))
        .unwrap();
        // Waiting on two, as its answer, a read and the listing all say:
        // not open, and no take takes it.
        assert_eq!(made["work"]["state"], "waiting", "{made}");
        let read = lead(json!({"read": "enum"})).unwrap();
        assert_eq!(read["work"]["state"], "waiting", "{read}");
        let talk = lead(json!({"reply": {"thread": "enum", "text": "Soon."}})).unwrap();
        assert_eq!(talk["work"]["state"], "waiting", "{talk}");
        let mut open = open_names(&home);
        open.sort();
        assert_eq!(open, ["api", "store"]);
        let worker = |args| send(&home, "shop.w1", "shop", None, false, args);
        let waiting = worker(act("enum", "take")).unwrap_err();
        assert!(waiting.starts_with("work_waiting"), "{waiting}");
        let taken = worker(act("store", "take")).unwrap();
        assert_eq!(taken["work"]["state"], "taken");
        assert_eq!(taken["work"]["holder"], "shop.w1");
        assert_eq!(open_names(&home), ["api"]);
        // Another's take of held work is refused while its holder works.
        let other = |args| send(&home, "shop.w2", "shop", None, false, args);
        assert!(
            other(act("store", "take"))
                .unwrap_err()
                .starts_with("work_held")
        );
        // Only the holder closes it.
        let closed = other(act("store", "done")).unwrap_err();
        assert!(closed.starts_with("work_not_holder"), "{closed}");
        let done = worker(act("store", "done")).unwrap();
        assert_eq!(done["work"]["state"], "done");
        assert_eq!(done["work"]["result"], "y");
        other(act("api", "take")).unwrap();
        // The last need done opens the enum, and its post says so.
        other(act("api", "done")).unwrap();
        let posts = commons::posts(&commons::dir(&home).join("api.jsonl")).unwrap();
        assert_eq!(posts.last().unwrap()["opened"], json!(["enum"]));
        assert_eq!(open_names(&home), ["enum"]);
        // Its taker hears what its needs made.
        let took = worker(act("enum", "take")).unwrap();
        let made: Vec<_> = (took["needs_made"].as_array().unwrap().iter())
            .map(|m| (m["thread"].as_str().unwrap(), m["by"].as_str().unwrap()))
            .collect();
        assert_eq!(made, [("store", "shop.w1"), ("api", "shop.w2")]);
        // Every step is a post in the thread, nothing rewritten.
        let posts = commons::posts(&commons::dir(&home).join("store.jsonl")).unwrap();
        let steps: Vec<_> = posts.iter().map(|p| p["do"].as_str().unwrap()).collect();
        assert_eq!(steps, ["open", "take", "done"]);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn needs_change_while_work_is_open_never_into_a_cycle() {
        let home = home("repoint");
        let lead = |args| send(&home, "shop.lead", "shop", None, false, args);
        lead(work("store", "Type the store errors", json!({}))).unwrap();
        lead(work("enum", "Remove the enum", json!({"needs": ["store"]}))).unwrap();
        let cycle = lead(set("store", json!({"needs": ["enum"]}))).unwrap_err();
        assert!(cycle.starts_with("work_invalid"), "{cycle}");
        // Needing nothing now, it opens, and its post says so.
        let freed = lead(set("enum", json!({"needs": []}))).unwrap();
        assert_eq!(freed["work"]["needs"], json!([]));
        let posts = commons::posts(&commons::dir(&home).join("enum.jsonl")).unwrap();
        assert_eq!(posts.last().unwrap()["do"], "set");
        assert_eq!(posts.last().unwrap()["opened"], json!(["enum"]));
        let mut open = open_names(&home);
        open.sort();
        assert_eq!(open, ["enum", "store"]);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn only_the_boards_project_works_it_and_anyone_posts() {
        let home = home("project");
        send(
            &home,
            "shop.lead",
            "shop",
            None,
            false,
            work("store", "Type the store errors", json!({})),
        )
        .unwrap();
        let docs = |args| send(&home, "docs.lead", "docs", None, false, args);
        assert!(
            docs(act("store", "take"))
                .unwrap_err()
                .starts_with("work_not_yours")
        );
        let labeled = docs(set("store", json!({"labels": {"kind": "bug"}}))).unwrap_err();
        assert!(labeled.starts_with("work_not_yours"), "{labeled}");
        // A reply with no work fields lands as talk.
        let said =
            docs(json!({"reply": {"thread": "store", "text": "Saw this in docs too"}})).unwrap();
        assert_eq!(said["posts"], 2);
        assert_eq!(open_names(&home), ["store"]);
        // A plain post goes on the board beside work.
        docs(
            json!({"post": {"name": "docs-note", "title": "Docs see it too", "text": "x",
            "board": "shop/errors"}}),
        )
        .unwrap();
        let mut on = listed(&home, json!({}));
        on.sort();
        assert_eq!(on, ["docs-note", "store"]);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_take_takes_over_from_a_closed_holder_and_says_so() {
        let home = home("takeover");
        send(
            &home,
            "shop.lead",
            "shop",
            None,
            false,
            work("store", "Type the store errors", json!({})),
        )
        .unwrap();
        send(&home, "shop.w1", "shop", None, false, act("store", "take")).unwrap();
        let took = send(&home, "shop.w2", "shop", None, true, act("store", "take")).unwrap();
        assert_eq!(took["work"]["holder"], "shop.w2");
        let posts = commons::posts(&commons::dir(&home).join("store.jsonl")).unwrap();
        assert_eq!(posts.last().unwrap()["took_from"], "shop.w1");
        // The closed holder follows it as one that posted there.
        assert_eq!(took["dropped_from"]["name"], "shop.w1");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn its_maker_or_the_lead_drops_work_from_a_live_holder() {
        let home = home("drop");
        let w1 = |args| send(&home, "shop.w1", "shop", None, false, args);
        let w2 = |args| send(&home, "shop.w2", "shop", None, false, args);
        let lead = |args| send(&home, "shop.lead", "shop", None, false, args);
        w1(work("store", "Type the store errors", json!({}))).unwrap();
        w2(act("store", "take")).unwrap();
        let refused = send(&home, "shop.w3", "shop", None, false, act("store", "drop"));
        assert!(refused.unwrap_err().starts_with("work_not_holder"));
        assert_eq!(w1(act("store", "drop")).unwrap()["work"]["state"], "open");
        w2(act("store", "take")).unwrap();
        let dropped = lead(act("store", "drop")).unwrap();
        assert_eq!(dropped["work"]["state"], "open");
        let posts = commons::posts(&commons::dir(&home).join("store.jsonl")).unwrap();
        assert_eq!(posts.last().unwrap()["opened"], json!(["store"]));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn work_for_agents_or_profiles_is_theirs_alone() {
        let home = home("reserved");
        let lead = |args| send(&home, "shop.lead", "shop", None, false, args);
        lead(work(
            "review",
            "Review the change",
            json!({"for": ["profile:reviewer"]}),
        ))
        .unwrap();
        let worker = send(
            &home,
            "shop.w1",
            "shop",
            Some("worker"),
            false,
            act("review", "take"),
        );
        assert!(worker.unwrap_err().starts_with("work_reserved"));
        send(
            &home,
            "shop.r",
            "shop",
            Some("reviewer"),
            false,
            act("review", "take"),
        )
        .unwrap();
        // `for` changes who may; empty is anyone.
        lead(work("docs", "Write the docs", json!({}))).unwrap();
        let named = lead(set("docs", json!({"for": ["shop.w9", "shop.w8"]}))).unwrap();
        assert_eq!(named["work"]["for"], json!(["shop.w9", "shop.w8"]));
        let refused = send(&home, "shop.w1", "shop", None, false, act("docs", "take"));
        assert!(refused.unwrap_err().starts_with("work_reserved"));
        assert!(lead(set("docs", json!({"for": []}))).unwrap()["work"]["for"].is_null());
        send(&home, "shop.w1", "shop", None, false, act("docs", "take")).unwrap();
        let bad = lead(work("x", "Do x", json!({"for": "reviewer"}))).unwrap_err();
        assert!(bad.starts_with("work_invalid"), "{bad}");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn titles_are_short_and_never_twice_on_a_board() {
        let home = home("titles");
        let lead = |args| send(&home, "shop.lead", "shop", None, false, args);
        let long = lead(work(
            "long",
            "Type every error in the store and the api",
            json!({}),
        ));
        assert!(long.unwrap_err().starts_with("work_invalid"));
        lead(work("store", "Type the store errors", json!({}))).unwrap();
        let twice = lead(work("store-2", "type the store errors", json!({})));
        assert!(twice.unwrap_err().starts_with("work_exists"));
        // Done, the same title is new work again.
        send(&home, "shop.w1", "shop", None, false, act("store", "take")).unwrap();
        send(&home, "shop.w1", "shop", None, false, act("store", "done")).unwrap();
        lead(work("store-2", "Type the store errors", json!({}))).unwrap();
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn labels_say_what_work_is_and_never_where_it_stands() {
        let home = home("labels");
        let lead = |args| send(&home, "shop.lead", "shop", None, false, args);
        let state = lead(work(
            "bug",
            "Fix the redirect",
            json!({"labels": {"state": "x"}}),
        ));
        assert!(state.unwrap_err().starts_with("work_invalid"));
        lead(work(
            "bug",
            "Fix the redirect",
            json!({"labels": {"kind": "bug", "area": "web"}}),
        ))
        .unwrap();
        lead(work(
            "feature",
            "Add dark mode",
            json!({"labels": {"kind": "feature"}}),
        ))
        .unwrap();
        assert_eq!(listed(&home, json!({"labels": {"kind": ["bug"]}})), ["bug"]);
        let changed = lead(set("bug", json!({"labels": {"area": null, "size": "s"}}))).unwrap();
        assert_eq!(
            changed["work"]["labels"],
            json!({"kind": "bug", "size": "s"})
        );
        // Labels go with work, not a plain post.
        let plain = lead(
            json!({"post": {"name": "note", "title": "A note", "text": "x",
            "board": "shop/errors", "labels": {"kind": "bug"}}}),
        );
        assert!(plain.unwrap_err().starts_with("work_invalid"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_thread_already_there_opens_as_work_and_its_maker_cancels_it() {
        let home = home("thread");
        send(
            &home,
            "shop.lead",
            "shop",
            None,
            false,
            json!({"post": {"name": "flaky", "title": "Fix the flaky test", "text": "x"}}),
        )
        .unwrap();
        let w1 = |args| send(&home, "shop.w1", "shop", None, false, args);
        let none = w1(act("flaky", "take")).unwrap_err();
        assert!(none.starts_with("work_not_work"), "{none}");
        let mut open = act("flaky", "open");
        open["reply"]["board"] = json!("shop/errors");
        assert_eq!(w1(open.clone()).unwrap()["work"]["state"], "open");
        assert!(w1(open).unwrap_err().starts_with("work_invalid"));
        // Its maker, w1, cancels it; the thread's starter does not.
        let lead = send(
            &home,
            "shop.lead",
            "shop",
            None,
            false,
            act("flaky", "fail"),
        );
        assert!(lead.unwrap_err().starts_with("work_not_holder"));
        assert_eq!(
            w1(act("flaky", "fail")).unwrap()["work"]["state"],
            "canceled"
        );
        assert!(
            w1(act("flaky", "take"))
                .unwrap_err()
                .starts_with("work_closed")
        );
        // A title too long for a board needs a short one.
        send(
            &home,
            "shop.lead",
            "shop",
            None,
            false,
            json!({"post": {"name": "slow", "title": "The build has been slow since Monday",
                "text": "x"}}),
        )
        .unwrap();
        let mut open = act("slow", "open");
        open["reply"]["board"] = json!("shop/errors");
        assert!(w1(open.clone()).unwrap_err().starts_with("work_invalid"));
        open["reply"]["title"] = json!("Speed up the build");
        assert_eq!(w1(open).unwrap()["work"]["title"], "Speed up the build");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn failed_work_opens_again_and_dropped_work_is_open() {
        let home = home("reopen");
        let w1 = |args| send(&home, "shop.w1", "shop", None, false, args);
        w1(work("store", "Type the store errors", json!({}))).unwrap();
        w1(act("store", "take")).unwrap();
        assert_eq!(w1(act("store", "drop")).unwrap()["work"]["state"], "open");
        w1(act("store", "take")).unwrap();
        assert_eq!(w1(act("store", "fail")).unwrap()["work"]["state"], "failed");
        assert!(open_names(&home).is_empty());
        assert_eq!(w1(act("store", "open")).unwrap()["work"]["state"], "open");
        assert_eq!(open_names(&home), ["store"]);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_step_after_posts_its_agent_missed_waits_once_as_a_reply_does() {
        let home = home("missed");
        send(
            &home,
            "shop.lead",
            "shop",
            None,
            false,
            work("store", "Type the store errors", json!({})),
        )
        .unwrap();
        let caller = Caller {
            name: "shop.w1",
            id: None,
            reader: "shop.w1",
            model: None,
            project: Some("shop"),
            profile: None,
            lead: false,
            folder: None,
        };
        // Never shown the work's post: held once, then taken.
        let held = commons::call_as(&home, &caller, &act("store", "take"), &|_, _| false).unwrap();
        assert_eq!(held["posted"], false);
        let taken = commons::call_as(&home, &caller, &act("store", "take"), &|_, _| false).unwrap();
        assert_eq!(taken["work"]["holder"], "shop.w1");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn the_listing_shows_where_work_stands_and_filters_on_it() {
        let home = home("listing");
        let lead = |args| send(&home, "shop.lead", "shop", None, false, args);
        lead(work("store", "Type the store errors", json!({}))).unwrap();
        lead(work(
            "enum",
            "Remove the old enum",
            json!({"needs": ["store"]}),
        ))
        .unwrap();
        let listed = lead(json!({"filter": {"state": ["waiting"]}})).unwrap();
        assert_eq!(listed["threads"].as_array().unwrap().len(), 1);
        assert_eq!(listed["threads"][0]["work"]["state"], "waiting");
        // The summary keeps the work as its posts make it.
        let summary = commons::summary("enum", &commons::dir(&home).join("enum.jsonl")).unwrap();
        assert_eq!(summary["work"]["state"], "open");
        assert_eq!(summary["work"]["needs"], json!(["store"]));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_text_saying_do_is_no_step() {
        let posts = [json!({"by": "a", "at": 1, "title": "T", "text": "\"do\":\"take\""})];
        assert!(standing(&posts).is_none());
        let line = posts[0].to_string();
        assert!(!line.contains(r#""do":"#));
    }

    #[test]
    fn news_names_works_board_and_what_it_opened() {
        let home = home("news");
        let cursor = home.join("cursor");
        let _ = commons::news(&home, &cursor).mark_read();
        send(
            &home,
            "shop.lead",
            "shop",
            None,
            false,
            work("store", "Type the store errors", json!({})),
        )
        .unwrap();
        let news = commons::news(&home, &cursor);
        let line: Value = serde_json::from_str(&news.lines[0]).unwrap();
        assert_eq!(line["board"], "shop/errors");
        assert_eq!(line["do"], "open");
        assert_eq!(line["opened"], json!(["store"]));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_board_is_a_projects_path() {
        assert!(valid_board("shop").is_ok());
        assert!(valid_board("shop.web/typed-errors").is_ok());
        // The project's name as the project has it, capitals and all.
        assert!(valid_board("MyApp/work").is_ok());
        for bad in [
            "",
            "shop/Errors",
            "shop//x",
            "shop/",
            "../x",
            "shop./x",
            "shop/x y",
        ] {
            assert!(valid_board(bad).is_err(), "{bad}");
        }
        assert_eq!(project_of("shop.web/typed-errors"), "shop.web");
    }

    #[test]
    fn a_read_and_a_search_see_work_as_the_listing_does() {
        let home = home("seen");
        let lead = |args| send(&home, "shop.lead", "shop", None, false, args);
        lead(work("store", "Type the store errors", json!({}))).unwrap();
        lead(work("enum", "Remove the enum", json!({"needs": ["store"]}))).unwrap();
        lead(act("store", "take")).unwrap();
        let commons =
            |args: Value| commons::call(&home, "x.y", None, "x.y", None, Some("x"), &args);
        // A read shows what each post did, and where the work stands.
        let read = commons(json!({"read": "store"})).unwrap();
        assert_eq!(read["work"]["state"], "taken");
        assert_eq!(read["posts"][0]["do"], "open");
        assert_eq!(read["posts"][0]["board"], "shop/errors");
        assert_eq!(read["posts"][1]["do"], "take");
        // A search filters on the work's state, waiting included.
        let found = |state: &str| {
            let found = commons(json!({"search": "", "filter": {"state": [state]}})).unwrap();
            (found["threads"].as_array().into_iter().flatten())
                .map(|t| t["thread"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(found("taken"), ["store"]);
        assert_eq!(found("waiting"), ["enum"]);
        assert!(found("open").is_empty());
        // Its own fields still stand: by is who wrote each post.
        let by = commons(json!({"search": "", "filter": {"by": ["shop.lead"], "do": ["take"]}}))
            .unwrap();
        assert_eq!(by["threads"][0]["work"]["state"], "taken");
    }

    #[test]
    fn a_board_follow_reads_back_whatever_the_board_s_length() {
        assert_eq!(follow("shop/errors", Some(7)).name, "board.shop.errors");
        let long = format!("shop/{}/{}", "a".repeat(64), "b".repeat(64));
        let one = follow(&long, Some(7));
        let back = Sub::from_json(&one.to_json()).expect("reads back");
        assert_eq!(back.name, one.name);
        assert_eq!(back.filter, Some(json!({"board": [long], "event": EVENTS})));
        assert_eq!(back.steer, Some(every()));
        assert!(!back.outlives_close);
    }

    #[test]
    fn only_its_maker_cancels_work_not_a_later_agent_of_its_name() {
        let home = home("cancel");
        let as_id = |id: i64, arguments: Value| {
            let caller = Caller {
                name: "shop.w1",
                id: Some(id),
                reader: "shop.w1",
                model: None,
                project: Some("shop"),
                profile: None,
                lead: false,
                folder: None,
            };
            if let Some(thread) = arguments["reply"]["thread"].as_str()
                && let Ok(posts) =
                    commons::posts(&commons::dir(&home).join(format!("{thread}.jsonl")))
            {
                commons::told(
                    &home,
                    "shop.w1",
                    thread,
                    &(0..posts.len()).collect::<Vec<_>>(),
                );
            }
            commons::call_as(&home, &caller, &arguments, &|_, _| false)
        };
        let made = as_id(1, work("store", "Type the store errors", json!({}))).unwrap();
        assert_eq!(made["work"]["by_id"], 1);
        let refused = as_id(2, act("store", "fail")).unwrap_err();
        assert!(refused.starts_with("work_not_holder"), "{refused}");
        let canceled = as_id(1, act("store", "fail")).unwrap();
        assert_eq!(canceled["work"]["state"], "canceled");
    }

    #[test]
    fn work_reserved_then_freed_lists_as_nobody_s() {
        let home = home("unreserved");
        let lead = |args| send(&home, "shop.lead", "shop", None, false, args);
        lead(work(
            "store",
            "Type the store errors",
            json!({"for": ["profile:reviewer"]}),
        ))
        .unwrap();
        lead(set("store", json!({"for": []}))).unwrap();
        assert!(listed(&home, json!({"for": ["profile:reviewer"]})).is_empty());
        assert_eq!(
            listed(&home, json!({"for": [{"exists": false}]})),
            ["store"]
        );
        // A search sees where it stands now too, never what a post once said.
        lead(set("store", json!({"for": ["shop.coder"]}))).unwrap();
        let found = |who: &str| {
            let filter = json!({"for": [who]});
            let found = commons::call(
                &home,
                "x.y",
                None,
                "x.y",
                None,
                Some("x"),
                &json!({"search": "", "filter": filter}),
            )
            .unwrap();
            found["threads"].as_array().map_or(0, Vec::len)
        };
        assert_eq!(found("profile:reviewer"), 0);
        assert_eq!(found("shop.coder"), 1);
    }

    #[test]
    fn a_full_thread_still_takes_what_moves_its_work() {
        let home = home("full");
        let lead = |args| send(&home, "shop.lead", "shop", None, false, args);
        lead(work("store", "Type the store errors", json!({}))).unwrap();
        lead(act("store", "take")).unwrap();
        let talk = json!({"by": "x.y", "at": 1, "text": "more"}).to_string() + "\n";
        let path = commons::dir(&home).join("store.jsonl");
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        std::io::Write::write_all(&mut file, talk.repeat(commons::POSTS).as_bytes()).unwrap();
        let refused = lead(set("store", json!({"labels": {"area": "db"}}))).unwrap_err();
        assert!(refused.starts_with("commons_full"), "{refused}");
        let done = lead(act("store", "done")).unwrap();
        assert_eq!(done["work"]["state"], "done");
    }

    #[test]
    fn failed_work_opens_again_only_while_no_other_holds_its_title() {
        let home = home("reopen-title");
        let lead = |args| send(&home, "shop.lead", "shop", None, false, args);
        lead(work("store", "Type the store errors", json!({}))).unwrap();
        lead(act("store", "take")).unwrap();
        lead(act("store", "fail")).unwrap();
        lead(work("store-2", "Type the store errors", json!({}))).unwrap();
        let refused = lead(act("store", "open")).unwrap_err();
        assert!(refused.starts_with("work_exists: store-2"), "{refused}");
        lead(act("store-2", "fail")).unwrap();
        lead(act("store", "open")).unwrap();
    }

    /// How long a board's listing takes with 50 boards and 1,000 pieces of
    /// work, half of them taken and a fifth done: run by hand, in release.
    #[test]
    #[ignore]
    fn listing_fifty_boards_of_a_thousand_pieces_of_work_takes() {
        let home = home("timing");
        let lead = |args| send(&home, "shop.lead", "shop", None, false, args);
        let made = std::time::Instant::now();
        for n in 0..1000 {
            let board = format!("shop/b{}", n % 50);
            let name = format!("i{n}");
            // A third need the work before them on their board, so
            // answers read whether their needs are done.
            let needs = match n % 3 == 1 && n >= 50 {
                true => json!({"needs": [format!("i{}", n - 50)]}),
                false => json!({}),
            };
            let mut post = work(&name, &format!("Fix item {n}"), needs);
            post["post"]["board"] = json!(board);
            lead(post).unwrap();
            if n % 2 == 0 && !(n % 3 == 1 && n >= 50) {
                lead(act(&name, "take")).unwrap();
            }
            if n % 10 == 0 && !(n % 3 == 1 && n >= 50) {
                lead(act(&name, "done")).unwrap();
            }
        }
        eprintln!("made 1,000 pieces of work in {:?}", made.elapsed());
        let read = std::time::Instant::now();
        for n in (51..1000).step_by(3) {
            lead(json!({"read": format!("i{n}")})).unwrap();
        }
        eprintln!("read the 317 that need work in {:?}", read.elapsed());
        for round in 0..3 {
            let started = std::time::Instant::now();
            let open = lead(json!({"filter": {"board": ["shop/b7"], "state": ["open"]}})).unwrap();
            let one = started.elapsed();
            let started = std::time::Instant::now();
            let all = commons::list(
                &commons::dir(&home),
                0,
                commons::LIST_BYTES,
                &mut commons::Summaries::new(),
                &[],
                Some(&json!({"state": ["open", "taken", "waiting"]})),
            )
            .unwrap();
            eprintln!(
                "round {round}: one board's open work ({}) in {one:?}; every board's work ({} listed) in {:?}",
                open["threads"].as_array().map_or(0, Vec::len),
                all["threads"].as_array().map_or(0, Vec::len),
                started.elapsed()
            );
        }
    }

    #[test]
    fn old_steps_take_todays_verbs_once_and_their_work_stands_as_it_did() {
        let home = home("renamed");
        let dir = commons::dir(&home);
        std::fs::create_dir_all(&dir).unwrap();
        let old = [
            json!({"by": "shop.lead", "at": 1, "title": "Review it", "text": "x",
                "board": "shop/errors", "do": "add", "for": "reviewer", "needs": []}),
            json!({"by": "shop.r", "at": 2, "text": "y", "do": "claim"}),
            json!({"by": "shop.r", "at": 3, "text": "y", "do": "release"}),
            json!({"by": "shop.lead", "at": 4, "text": "y", "do": "label",
                "labels": {"kind": "review"}}),
            json!({"by": "shop.lead", "at": 5, "text": "y", "do": "assign", "for": ""}),
            json!({"by": "shop.lead", "at": 6, "text": "y", "do": "cancel"}),
            json!({"by": "shop.lead", "at": 7, "text": "y", "do": "reopen"}),
            json!({"by": "x.y", "at": 8, "text": "talk"}),
        ];
        let text: String = old.iter().map(|post| format!("{post}\n")).collect();
        std::fs::write(dir.join("review.jsonl"), text).unwrap();
        steps_renamed(&home);
        let posts = commons::posts(&dir.join("review.jsonl")).unwrap();
        let steps: Vec<_> = posts.iter().map(|p| p["do"].as_str()).collect();
        assert_eq!(
            steps,
            [
                Some("open"),
                Some("take"),
                Some("drop"),
                Some("set"),
                Some("set"),
                Some("fail"),
                Some("open"),
                None
            ]
        );
        assert_eq!(posts[0]["for"], json!(["reviewer", "profile:reviewer"]));
        assert_eq!(posts[4]["for"], json!([]));
        let work = standing(&posts).unwrap();
        assert_eq!(work["state"], "open");
        assert_eq!(work["labels"], json!({"kind": "review"}));
        assert!(work["for"].is_null());
        // A reviewer made as the profile takes work kept for it, as before.
        let kept = standing(&posts[..1]).unwrap();
        assert!(is_for(&kept, "shop.r", Some("reviewer")));
        assert!(is_for(&kept, "reviewer", None));
        assert!(!is_for(&kept, "shop.w1", Some("worker")));
        let record = home.join(".agent/sonny/migrated");
        assert_eq!(crate::upgrade::lines(&record), [STEPS_RENAMED]);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_mention_names_agents_at_a_word_s_start_and_never_an_email() {
        let said =
            |text: &str| mentioned(&json!({"reply": {"thread": "t", "text": text}}), "shop.w1");
        assert_eq!(
            said("@shop.review, does this handle retries? cc @billing.api."),
            ["shop.review", "billing.api"]
        );
        assert!(said("mail ana@shop.com or see @types/node and @everyone").is_empty());
        assert!(said("me, @shop.w1, and @shop.w1 again").is_empty());
        assert_eq!(said("(@shop.lead) and @shop.lead"), ["shop.lead"]);
    }

    #[test]
    fn a_board_s_news_names_each_step_and_says_when_it_empties_or_goes_quiet() {
        let home = home("events");
        let cursor = home.join("cursor");
        let _ = commons::news(&home, &cursor).mark_read();
        let lead = |args| send(&home, "shop.lead", "shop", None, false, args);
        lead(work("store", "Type the store errors", json!({}))).unwrap();
        lead(work("enum", "Remove the enum", json!({"needs": ["store"]}))).unwrap();
        lead(
            json!({"post": {"name": "idea", "title": "An idea", "text": "x",
            "board": "shop/errors"}}),
        )
        .unwrap();
        lead(json!({"reply": {"thread": "idea", "text": "more"}})).unwrap();
        let w1 = |args| send(&home, "shop.w1", "shop", None, false, args);
        w1(act("store", "take")).unwrap();
        w1(act("store", "done")).unwrap();
        w1(act("enum", "take")).unwrap();
        let events = |news: &crate::lookout::sources::News| -> Vec<(String, String)> {
            (news.lines.iter())
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .map(|line| {
                    let event = line["event"].as_str().unwrap_or_default().to_owned();
                    let topic = line["topic"].as_str().unwrap_or("-").to_owned();
                    (topic, event)
                })
                .collect()
        };
        let news = commons::news(&home, &cursor);
        let mut said = events(&news);
        said.sort();
        let pairs = |list: &[(&str, &str)]| -> Vec<(String, String)> {
            let mut list: Vec<_> = (list.iter())
                .map(|(t, e)| ((*t).to_owned(), (*e).to_owned()))
                .collect();
            list.sort();
            list
        };
        assert_eq!(
            said,
            pairs(&[
                ("store", "opened"),
                ("store", "taken"),
                ("store", "done"),
                ("enum", "waiting"),
                ("enum", "taken"),
                ("idea", "posted"),
                ("idea", "reply"),
            ])
        );
        // The done line says what it opened.
        let done = (news.lines.iter())
            .find(|line| line.contains("\"done\"") && line.contains("\"store\""))
            .unwrap();
        assert!(done.contains("It opened enum."), "{done}");
        let _ = news.mark_read();
        // The last work settling empties the board: its own line, on no
        // thread.
        w1(act("enum", "done")).unwrap();
        let news = commons::news(&home, &cursor);
        let mut said = events(&news);
        said.sort();
        assert_eq!(said, pairs(&[("enum", "done"), ("-", "emptied")]));
        let _ = news.mark_read();
        // Held work with no post for hold_quiet goes quiet, once.
        lead(work("api", "Type the api errors", json!({}))).unwrap();
        w1(act("api", "take")).unwrap();
        let path = commons::dir(&home).join("api.jsonl");
        let aged: String = (commons::posts(&path).unwrap().into_iter())
            .map(|mut post| {
                post["at"] = json!(post["at"].as_i64().unwrap() - 3600);
                format!("{post}\n")
            })
            .collect();
        std::fs::write(&path, aged).unwrap();
        let news = commons::news(&home, &cursor);
        assert!(events(&news).contains(&("-".to_owned(), "quiet".to_owned())));
        let _ = news.mark_read();
        assert!(commons::news(&home, &cursor).lines.is_empty());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_take_reads_the_needs_and_for_its_own_reply_gives() {
        let home = home("take-fields");
        let lead = |args| send(&home, "shop.lead", "shop", None, false, args);
        lead(work("store", "Type the store errors", json!({}))).unwrap();
        lead(work("api", "Type the api errors", json!({}))).unwrap();
        let w = |args| send(&home, "shop.w1", "shop", None, false, args);
        // Needing undone work in the same reply refuses the take.
        let mut take = act("api", "take");
        take["reply"]["needs"] = json!(["store"]);
        let refused = w(take).unwrap_err();
        assert!(refused.starts_with("work_waiting"), "{refused}");
        // Kept for someone else in the same reply, it isn't this agent's.
        let mut take = act("api", "take");
        take["reply"]["for"] = json!(["shop.other"]);
        let refused = w(take).unwrap_err();
        assert!(refused.starts_with("work_reserved"), "{refused}");
        assert!(w(act("api", "take")).is_ok());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_thread_opened_as_work_lists_with_the_short_title_it_was_given() {
        let home = home("retitled");
        let lead = |args| send(&home, "shop.lead", "shop", None, false, args);
        lead(json!({"post": {"name": "long", "title": "A long question about how errors should carry causes",
            "text": "x"}}))
        .unwrap();
        lead(
            json!({"reply": {"thread": "long", "do": "open", "board": "shop/errors",
            "title": "Carry error causes", "text": "y"}}),
        )
        .unwrap();
        let summary = commons::summary("long", &commons::dir(&home).join("long.jsonl")).unwrap();
        assert_eq!(summary["work"]["title"], "Carry error causes", "{summary}");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_tasks_board_stays_its_own_when_its_agent_takes_work_elsewhere() {
        let home = home("works-from");
        let store = home.join("store");
        crate::lookout::subs::put(&store, "shop.w1", works_from("shop/a", Some(2))).unwrap();
        let lead = |args| send(&home, "shop.lead", "shop", None, false, args);
        for board in ["shop/a", "shop/b"] {
            let mut opened = work(board.replace('/', "-").as_str(), "Do it", json!({}));
            opened["post"]["board"] = json!(board);
            opened["post"]["title"] = json!(format!("Do {board}"));
            lead(opened).unwrap();
        }
        for thread in ["shop-b", "shop-a"] {
            let take = act(thread, "take");
            let said = send(&home, "shop.w1", "shop", None, false, take.clone()).unwrap();
            follows(&store, "shop.w1", Some(2), &take, &said).unwrap();
            assert_eq!(followed(&store, "shop.w1").as_deref(), Some("shop/a"));
        }
        // Starting a helper there keeps it marked too.
        follow_kept(&store, "shop.w1", "shop/a", Some(2));
        assert_eq!(followed(&store, "shop.w1").as_deref(), Some("shop/a"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn taking_work_follows_its_board_and_its_thread_as_its_holder() {
        let home = home("follows");
        let store = home.join("store");
        let lead = |args| send(&home, "shop.lead", "shop", None, false, args);
        let opened = work("store", "Type the store errors", json!({}));
        let said = lead(opened.clone()).unwrap();
        let mine = follows(&store, "shop.lead", Some(1), &opened, &said).unwrap();
        assert_eq!(mine.about.as_deref(), Some(POSTED));
        assert_eq!(mine.steer, Some(every()));
        assert_eq!(
            followed(&store, "shop.lead").as_deref(),
            Some("shop/errors")
        );
        let take = act("store", "take");
        let said = send(&home, "shop.w1", "shop", None, false, take.clone()).unwrap();
        let held = follows(&store, "shop.w1", Some(2), &take, &said).unwrap();
        assert_eq!(held.about.as_deref(), Some(HOLDS));
        assert_eq!(held.quiet.as_deref(), Some("15m"));
        // A reply that moves nothing keeps its holder following as one.
        let talk = json!({"reply": {"thread": "store", "text": "halfway"}});
        let said = send(&home, "shop.w1", "shop", None, false, talk.clone()).unwrap();
        let still = follows(&store, "shop.w1", Some(2), &talk, &said).unwrap();
        assert_eq!(still.about.as_deref(), Some(HOLDS));
        assert_eq!(still.quiet.as_deref(), Some("15m"));
        assert_eq!(followed(&store, "shop.w1").as_deref(), Some("shop/errors"));
        // Dropped by the lead, its holder's follow says it posted there.
        crate::lookout::subs::put(&store, "shop.w1", held).unwrap();
        let drop = act("store", "drop");
        let said = lead(drop.clone()).unwrap();
        assert_eq!(said["dropped_from"]["name"], "shop.w1");
        follows(&store, "shop.lead", Some(1), &drop, &said).unwrap();
        let after = crate::lookout::subs::read(&store, "shop.w1");
        let thread = after
            .iter()
            .find(|sub| sub.name == "commons.store")
            .unwrap();
        assert_eq!(thread.about.as_deref(), Some(POSTED));
        assert_eq!(thread.quiet, None);
        // A plain thread on no board follows as any commons thread.
        let plain = json!({"post": {"name": "q", "title": "A question", "text": "x"}});
        let said = lead(plain.clone()).unwrap();
        assert!(follows(&store, "shop.lead", Some(1), &plain, &said).is_none());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn done_records_its_authors_checkout_and_the_work_needing_it_hears_it() {
        let home = home("checkout");
        let repo = home.join("worktrees/shop.b.1");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };
        git(&["init", "-q", "-b", "agent/b.1"]);
        // Untracked files count even where the config hides them.
        git(&["config", "status.showUntrackedFiles", "no"]);
        std::fs::write(repo.join("store.rs"), "typed").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "store"]);
        let commit = git(&["rev-parse", "HEAD"]);
        // A builder in that checkout, as the tool host passes its folder.
        let builder = |args: Value| {
            if let Some(thread) = args["reply"]["thread"].as_str()
                && let Ok(posts) =
                    commons::posts(&commons::dir(&home).join(format!("{thread}.jsonl")))
            {
                commons::told(
                    &home,
                    "shop.b.1",
                    thread,
                    &(0..posts.len()).collect::<Vec<_>>(),
                );
            }
            let caller = Caller {
                name: "shop.b.1",
                id: None,
                reader: "shop.b.1",
                model: None,
                project: Some("shop"),
                profile: None,
                lead: false,
                folder: Some(&repo),
            };
            commons::call_as(&home, &caller, &args, &|_, _| false)
        };
        let lead = |args| send(&home, "shop.lead", "shop", None, false, args);
        lead(work("store", "Type the store errors", json!({}))).unwrap();
        lead(work(
            "review",
            "Review the store errors",
            json!({"needs": ["store"]}),
        ))
        .unwrap();
        builder(act("store", "take")).unwrap();
        // Changes left out of the commit are said, not refused.
        std::fs::write(repo.join("draft.rs"), "half").unwrap();
        let done = builder(act("store", "done")).unwrap();
        assert_eq!(done["work"]["branch"], "agent/b.1");
        assert_eq!(done["work"]["commit"], commit.as_str());
        assert_eq!(done["work"]["uncommitted"], true);
        // It is the builder's own post that carries it.
        let posts = commons::posts(&commons::dir(&home).join("store.jsonl")).unwrap();
        let last = posts.last().unwrap();
        assert_eq!(
            (last["by"].as_str(), last["do"].as_str()),
            (Some("shop.b.1"), Some("done"))
        );
        assert_eq!(last["branch"], "agent/b.1");
        let read = send(&home, "x.y", "x", None, false, json!({"read": "store"})).unwrap();
        let shown = read["posts"].as_array().unwrap().last().unwrap().clone();
        assert_eq!(shown["commit"], commit.as_str());
        // The reviewer taking work that needs it hears where it is.
        let took = send(
            &home,
            "shop.r.1",
            "shop",
            None,
            false,
            act("review", "take"),
        )
        .unwrap();
        let made = &took["needs_made"][0];
        assert_eq!(made["thread"], "store");
        assert_eq!(made["branch"], "agent/b.1");
        assert_eq!(made["commit"], commit.as_str());
        assert_eq!(made["uncommitted"], true);
        // A done from no checkout records nothing.
        let other = |args| send(&home, "shop.b.2", "shop", None, false, args);
        lead(work("api", "Type the api errors", json!({}))).unwrap();
        other(act("api", "take")).unwrap();
        let done = other(act("api", "done")).unwrap();
        for key in MADE {
            assert!(done["work"][key].is_null(), "{key}: {done}");
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn its_maker_or_the_lead_pauses_open_work_and_only_they_open_it() {
        let home = home("pause");
        let lead = |args| send(&home, "shop.lead", "shop", None, false, args);
        let w1 = |args| send(&home, "shop.w1", "shop", None, false, args);
        lead(work("store", "Type the store errors", json!({}))).unwrap();
        lead(work(
            "api",
            "Type the api errors",
            json!({"needs": ["store"]}),
        ))
        .unwrap();
        let refused = w1(act("store", "pause")).unwrap_err();
        assert!(refused.starts_with("work_not_maker"), "{refused}");
        let paused = lead(act("store", "pause")).unwrap();
        assert_eq!(paused["work"]["state"], "paused", "{paused}");
        // Never offered: not open, and no one takes it.
        assert!(open_names(&home).is_empty());
        assert_eq!(listed(&home, json!({"state": ["paused"]})), ["store"]);
        let taken = w1(act("store", "take")).unwrap_err();
        assert!(taken.starts_with("work_paused"), "{taken}");
        // Paused work is still to be done: its title stays its own.
        let twice = lead(work("store2", "Type the store errors", json!({}))).unwrap_err();
        assert!(twice.starts_with("work_exists"), "{twice}");
        // Only its maker or the lead opens it again.
        let opened = w1(act("store", "open")).unwrap_err();
        assert!(opened.starts_with("work_not_maker"), "{opened}");
        let opened = lead(act("store", "open")).unwrap();
        assert_eq!(opened["work"]["state"], "open", "{opened}");
        assert_eq!(open_names(&home), ["store"]);
        // Work waiting on its needs pauses too, and its needs done leave
        // it paused; opened, it is open.
        lead(act("api", "pause")).unwrap();
        w1(act("store", "take")).unwrap();
        let done = w1(act("store", "done")).unwrap();
        assert!(done.get(ASK_FOR).is_none(), "{done}");
        assert_eq!(listed(&home, json!({"state": ["paused"]})), ["api"]);
        // A board with paused work left is not emptied.
        let posts = commons::posts(&commons::dir(&home).join("store.jsonl")).unwrap();
        assert!(posts.last().unwrap()["emptied"].is_null());
        assert_eq!(event_of(&posts, 0, "store"), "opened");
        assert_eq!(event_of(&posts, 1, "store"), "paused");
        let opened = lead(act("api", "open")).unwrap();
        assert_eq!(opened["work"]["state"], "open", "{opened}");
        // Taken work is its holder's to drop, never paused.
        w1(act("api", "take")).unwrap();
        let held = lead(act("api", "pause")).unwrap_err();
        assert!(held.starts_with("work_invalid"), "{held}");
        // A worker pauses the work it made; failing it then cancels it.
        w1(work("docs", "Write the error docs", json!({}))).unwrap();
        w1(act("docs", "pause")).unwrap();
        let canceled = w1(act("docs", "fail")).unwrap();
        assert_eq!(canceled["work"]["state"], "canceled", "{canceled}");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn works_threads_end_with_their_agent_and_plain_threads_outlast_it() {
        let home = home("work-close");
        let store = home.join("store");
        let lead = |args| send(&home, "shop.lead", "shop", None, false, args);
        let opened = work("store", "Type the store errors", json!({}));
        let said = lead(opened.clone()).unwrap();
        let work_follow = follows(&store, "shop.lead", Some(1), &opened, &said).unwrap();
        assert!(!work_follow.outlives_close);
        let plain = json!({"post": {"name": "idea", "title": "An idea",
            "text": "x", "board": "shop/errors"}});
        let said = lead(plain.clone()).unwrap();
        let plain_follow = follows(&store, "shop.lead", Some(1), &plain, &said).unwrap();
        assert!(plain_follow.outlives_close);
        assert!(
            !Sub::from_json(&work_follow.to_json())
                .unwrap()
                .outlives_close
        );
        crate::lookout::subs::put(&store, "shop.lead", work_follow).unwrap();
        crate::lookout::subs::put(&store, "shop.lead", plain_follow).unwrap();
        assert_eq!(
            crate::lookout::subs::closed(&store, "shop.lead", Some(1)),
            Ok(2)
        );
        let kept: Vec<String> = (crate::lookout::subs::read(&store, "shop.lead").into_iter())
            .map(|sub| sub.name)
            .collect();
        assert_eq!(kept, ["commons.idea"]);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn follows_of_works_threads_written_before_end_with_their_agent_once() {
        let home = home("work-close-migrate");
        let store = home.join("store");
        let lead = |args| send(&home, "shop.lead", "shop", None, false, args);
        lead(work("store", "Type the store errors", json!({}))).unwrap();
        lead(json!({"post": {"name": "idea", "title": "An idea",
            "text": "x", "board": "shop/errors"}}))
        .unwrap();
        // As 0.10.4 wrote them: each lasting while its agent is closed.
        let before = |thread: &str| {
            let mut sub = post_follow(thread, Some(1), POSTED, None);
            sub.outlives_close = true;
            sub
        };
        crate::lookout::subs::put(&store, "shop.lead", before("store")).unwrap();
        crate::lookout::subs::put(&store, "shop.lead", before("idea")).unwrap();
        crate::lookout::subs::put(&store, "shop.lead", Sub::commons("q", Some(1))).unwrap();
        work_follows_close(&store, &home);
        let mut outlast: Vec<String> = (crate::lookout::subs::read(&store, "shop.lead")
            .into_iter())
        .filter(|sub| sub.outlives_close)
        .map(|sub| sub.name)
        .collect();
        outlast.sort();
        assert_eq!(outlast, ["commons.idea", "commons.q"]);
        // Once: a follow written afterwards as before stays as it is.
        crate::lookout::subs::put(&store, "shop.lead", before("store")).unwrap();
        work_follows_close(&store, &home);
        assert!(
            (crate::lookout::subs::read(&store, "shop.lead").iter())
                .any(|sub| sub.name == "commons.store" && sub.outlives_close)
        );
        let _ = std::fs::remove_dir_all(&home);
        let mut record = store.as_os_str().to_owned();
        record.push(".migrated");
        let _ = std::fs::remove_file(record);
    }
}
