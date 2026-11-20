//! Boards: work an agent opens in the commons for others to take. A board
//! is a path whose first segment is a project, as `shop` or
//! `shop/typed-errors`, made by its first item and never moved. An item
//! is a commons thread on a board, with work state. Each action on an item
//! is a post Sonny marks with `do`, so its thread keeps every step, and
//! where an item stands is what its posts say, read from them each time,
//! never kept beside them. Anyone may read an item and post in its thread;
//! only the agents of the board's project take one. The board tool takes
//! what the commons tool takes for the same step, plus the board's own:
//! `add` as `post`, `act` as `reply`, `open` as the listing.
use crate::commons;
use serde_json::{Value, json};
use std::path::Path;

/// The tool's name.
pub const NAME: &str = "board";

/// What an action post says it does: `add` starts an item's thread and
/// `item` puts a thread already there on a board; the rest move it.
pub const ACTIONS: [&str; 10] = [
    "add", "item", "claim", "done", "fail", "release", "cancel", "reopen", "label", "assign",
];

/// The most of an item's title, as of a task's: its verb first.
pub const TITLE_CHARS: usize = 30;

/// Names a label never takes: what Sonny writes of an item.
pub const PROTECTED: [&str; 7] = ["state", "holder", "board", "by", "project", "at", "do"];

/// The project a board is the work of: its first segment.
pub fn project_of(board: &str) -> &str {
    board.split('/').next().unwrap_or(board)
}

/// A board is segments of a-z 0-9 - . _, each 1-64 of them, joined by /.
pub fn valid_board(board: &str) -> Result<(), String> {
    let segment = |s: &str| {
        (1..=64).contains(&s.len())
            && !s.starts_with('.')
            && (s.bytes()).all(|b| {
                b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.' || b == b'_'
            })
    };
    match !board.is_empty() && board.split('/').all(segment) {
        true => Ok(()),
        false => Err(format!(
            "board_invalid: a board is its project's name, then /-separated parts of a-z 0-9 - . _, \
             as shop/typed-errors; not {board:?}"
        )),
    }
}

/// An item's title: one line of at most [`TITLE_CHARS`], as a task's.
pub fn valid_title(title: &str) -> Result<(), String> {
    let fine = !title.trim().is_empty()
        && !title.contains('\n')
        && title.trim().chars().count() <= TITLE_CHARS;
    match fine {
        true => Ok(()),
        false => Err(format!(
            "board_invalid: an item's title is one line of at most {TITLE_CHARS} characters, its \
             verb first, as Fix the login redirect; not {title:?}"
        )),
    }
}

/// Where the item a thread's posts make stands, from its action posts in
/// order: None for a thread no `add` or `item` put on a board. Its
/// `state` is open, claimed, done, failed or canceled; whether an open one
/// waits on its needs is [`waiting`]'s, which knows the other items.
///
/// `{board, state, by, at, labels, needs, for?, holder?, holder_id?, result?}`:
/// `by` made it, `at` is its last action's time, `holder` holds a claimed
/// one and did a done or failed one, `result` is a closing post's text.
pub fn item(posts: &[Value]) -> Option<Value> {
    let mut item: Option<Value> = None;
    for post in posts {
        let Some(action) = post["do"].as_str() else {
            continue;
        };
        if action == "add" || action == "item" {
            // An item is made once; a second making is no action.
            if item.is_none() {
                let mut made = json!({"board": post["board"], "state": "open", "by": post["by"],
                    "at": post["at"], "labels": {}, "needs": []});
                if action == "add" {
                    if post["labels"].is_object() {
                        made["labels"] = post["labels"].clone();
                    }
                    if post["needs"].is_array() {
                        made["needs"] = post["needs"].clone();
                    }
                    if post["for"].is_string() {
                        made["for"] = post["for"].clone();
                    }
                }
                item = Some(made);
            }
            continue;
        }
        let Some(it) = item.as_mut().and_then(Value::as_object_mut) else {
            continue;
        };
        let mut set = |key: &str, value: Value| _ = it.insert(key.to_owned(), value);
        match action {
            "claim" => {
                set("state", json!("claimed"));
                set("holder", post["by"].clone());
                set("holder_id", post["by_id"].clone());
                set("result", Value::Null);
            }
            "done" | "fail" => {
                set(
                    "state",
                    json!(if action == "done" { "done" } else { "failed" }),
                );
                set("result", post["text"].clone());
            }
            "cancel" => {
                set("state", json!("canceled"));
                set("holder", Value::Null);
                set("holder_id", Value::Null);
                set("result", post["text"].clone());
            }
            "release" | "reopen" => {
                set("state", json!("open"));
                set("holder", Value::Null);
                set("holder_id", Value::Null);
                set("result", Value::Null);
            }
            "label" => {
                let labels = it.entry("labels").or_insert_with(|| json!({}));
                for (key, value) in post["labels"].as_object().into_iter().flatten() {
                    match value {
                        Value::Null => _ = labels.as_object_mut().map(|l| l.remove(key)),
                        value => labels[key] = value.clone(),
                    }
                }
            }
            "assign" => match post["for"].as_str().filter(|f| !f.is_empty()) {
                Some(f) => set("for", json!(f)),
                None => set("for", Value::Null),
            },
            _ => continue,
        }
        it.insert("at".into(), post["at"].clone());
        it.retain(|_, value| !value.is_null());
    }
    item
}

/// Whether `item` waits: open, with a need that `done` says is not done.
/// The listing and the board's own page alike ask this of an item, with
/// what they know of the others.
pub fn waiting(item: &Value, done: &dyn Fn(&str) -> bool) -> bool {
    item["state"] == "open"
        && (item["needs"].as_array().into_iter().flatten())
            .any(|need| !need.as_str().is_some_and(done))
}

/// Who calls the tool: the agent, the daemon's id for it, whom what it is
/// shown counts for (as `commons::who` names it), the model it writes
/// with, the project it is named in, and the profile it was made as.
pub struct Caller<'a> {
    pub name: &'a str,
    pub id: Option<i64>,
    pub reader: &'a str,
    pub model: Option<&'a str>,
    pub project: Option<&'a str>,
    pub profile: Option<&'a str>,
}

/// The board tool's answer for `caller`. `gone` says whether a holder,
/// by name and id, is closed or no longer there: a claim then takes its
/// item over.
pub fn call(
    home: &Path,
    caller: &Caller,
    arguments: &Value,
    gone: &dyn Fn(&str, Option<i64>) -> bool,
) -> Result<Value, String> {
    let dir = commons::dir(home);
    let given = ["open", "add", "act"]
        .iter()
        .filter(|key| !arguments[**key].is_null())
        .count();
    if given != 1 {
        return Err("board_invalid: give one of open, add or act".into());
    }
    if let Some(board) = arguments["open"].as_str() {
        return open(&dir, board, arguments);
    }
    if arguments["add"].is_object() {
        return add(&dir, caller, &arguments["add"]);
    }
    if arguments["act"].is_object() {
        return act(&dir, caller, &arguments["act"], gone);
    }
    Err("board_invalid: open is a board's name, add and act are objects".into())
}

/// The open items on `board`, newest first, as the commons lists threads.
fn open(dir: &Path, board: &str, arguments: &Value) -> Result<Value, String> {
    valid_board(board)?;
    let mut filter = match arguments.get("filter").filter(|f| !f.is_null()) {
        Some(filter) => {
            commons::check_filter(filter, "board_invalid")?;
            filter.clone()
        }
        None => json!({}),
    };
    filter["board"] = json!([board]);
    filter["state"] = json!(["open"]);
    let from = arguments["from"].as_u64().unwrap_or(0) as usize;
    let mut listed = commons::list(
        dir,
        from,
        commons::LIST_BYTES,
        &mut commons::Summaries::new(),
        &[],
        Some(&filter),
    )?;
    if let Some(listed) = listed.as_object_mut() {
        listed.remove("invites");
        listed.remove("dir");
        listed.insert("board".into(), json!(board));
    }
    Ok(listed)
}

/// Each item's summary in the commons, by thread, read under the lock the
/// caller holds.
fn items(dir: &Path) -> Vec<(String, Value)> {
    (commons::summaries(dir).into_iter())
        .filter(|(_, thread)| thread["item"].is_object())
        .collect()
}

/// An item still to be done on `board` titled `title` but for case: a
/// second would be the same work twice.
fn same_title<'a>(items: &'a [(String, Value)], board: &str, title: &str) -> Option<&'a str> {
    let title = title.trim().to_lowercase();
    (items.iter())
        .find(|(_, thread)| {
            thread["item"]["board"] == board
                && matches!(thread["item"]["state"].as_str(), Some("open" | "claimed"))
                && thread["title"]
                    .as_str()
                    .is_some_and(|t| t.trim().to_lowercase() == title)
        })
        .map(|(name, _)| name.as_str())
}

/// The labels given, each a string, none of them a name Sonny writes; with
/// `remove`, a null removes one.
fn labels(given: &Value, remove: bool) -> Result<serde_json::Map<String, Value>, String> {
    let Some(given) = given.as_object() else {
        return Err("board_invalid: labels are an object of names to strings".into());
    };
    for (key, value) in given {
        if PROTECTED.contains(&key.as_str()) || key.is_empty() {
            return Err(format!(
                "board_invalid: {key:?} is no label's: Sonny writes {}",
                PROTECTED.join(", ")
            ));
        }
        if !(value.is_string() || (remove && value.is_null())) {
            return Err(format!(
                "board_invalid: label {key} is a string{}",
                if remove { ", or null to remove it" } else { "" }
            ));
        }
    }
    Ok(given.clone())
}

fn add(dir: &Path, caller: &Caller, given: &Value) -> Result<Value, String> {
    let text = |key: &str| given[key].as_str().unwrap_or_default();
    let (name, title, board) = (text("name"), text("title"), text("board"));
    commons::valid_name(name)?;
    valid_title(title)?;
    valid_board(board)?;
    let needs: Vec<String> = match &given["needs"] {
        Value::Null => Vec::new(),
        Value::Array(needs) => (needs.iter())
            .map(|need| need.as_str().map(str::to_owned))
            .collect::<Option<_>>()
            .ok_or("board_invalid: needs are the names of items on the board")?,
        _ => return Err("board_invalid: needs are the names of items on the board".into()),
    };
    let mut post = commons::entry(
        caller.name,
        caller.id,
        caller.model,
        caller.project,
        text("text"),
        given,
    )?;
    post["do"] = json!("add");
    post["board"] = json!(board);
    if !needs.is_empty() {
        post["needs"] = json!(needs);
    }
    if !given["labels"].is_null() {
        let labels = labels(&given["labels"], false)?;
        if !labels.is_empty() {
            post["labels"] = Value::Object(labels);
        }
    }
    if let Some(f) = given["for"].as_str().filter(|f| !f.trim().is_empty()) {
        post["for"] = json!(f.trim());
    }
    let _lock = commons::lock(dir)?;
    let items = items(dir);
    if let Some(same) = same_title(&items, board, title) {
        return Err(format!(
            "board_exists: {same} on {board} is \"{}\" already; take it, or post there",
            title.trim()
        ));
    }
    let state = |thread: &str| {
        (items.iter())
            .find(|(name, item)| name == thread && item["item"]["board"] == board)
            .map(|(_, item)| {
                item["item"]["state"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned()
            })
    };
    let mut open = true;
    for need in &needs {
        match state(need) {
            Some(state) => open &= state == "done",
            None => {
                return Err(format!(
                    "board_invalid: {need} is no item on {board}; add it first"
                ));
            }
        }
    }
    if open {
        post["opened"] = json!([name]);
    }
    let mut said = commons::start_locked(dir, name, title.trim(), &post, caller.reader)?;
    said["item"] = item(std::slice::from_ref(&post)).unwrap_or_default();
    Ok(said)
}

fn act(
    dir: &Path,
    caller: &Caller,
    given: &Value,
    gone: &dyn Fn(&str, Option<i64>) -> bool,
) -> Result<Value, String> {
    let text = |key: &str| given[key].as_str().unwrap_or_default();
    let (thread, action) = (text("thread"), text("do"));
    commons::valid_name(thread)?;
    if action == "add" || !ACTIONS.contains(&action) {
        return Err(format!(
            "board_invalid: do is one of {}",
            ACTIONS[1..].join(", ")
        ));
    }
    let mut post = commons::entry(
        caller.name,
        caller.id,
        caller.model,
        caller.project,
        text("text"),
        given,
    )?;
    post["do"] = json!(action);
    let _lock = commons::lock(dir)?;
    let posts = commons::posts(&dir.join(format!("{thread}.jsonl")))?;
    let current = item(&posts);
    let ours = |board: &str| caller.project == Some(project_of(board));
    let not_ours = |board: &str, what: &str| {
        format!(
            "board_not_yours: only {}'s agents {what} on {board}; post in the thread to help",
            project_of(board)
        )
    };
    match (action, &current) {
        ("item", Some(item)) => {
            return Err(format!(
                "board_exists: {thread} is on {} already",
                item["board"].as_str().unwrap_or_default()
            ));
        }
        ("item", None) => {
            let board = text("board");
            valid_board(board)?;
            let title = posts[0]["title"].as_str().unwrap_or_default();
            valid_title(title)
                .map_err(|e| format!("{e}: add a new item that names this thread instead"))?;
            if let Some(same) = same_title(&items(dir), board, title) {
                return Err(format!(
                    "board_exists: {same} on {board} is \"{title}\" already"
                ));
            }
            post["board"] = json!(board);
            post["opened"] = json!([thread]);
        }
        (_, None) => {
            return Err(format!(
                "board_not_item: {thread} is on no board; act item puts it on one first"
            ));
        }
        (_, Some(it)) => {
            let board = it["board"].as_str().unwrap_or_default();
            let state = it["state"].as_str().unwrap_or_default();
            let holder = it["holder"].as_str();
            let holder_id = it["holder_id"].as_i64();
            let holds = holder == Some(caller.name)
                && (holder_id.is_none() || caller.id.is_none() || holder_id == caller.id);
            match action {
                "claim" => {
                    if !ours(board) {
                        return Err(not_ours(board, "take items"));
                    }
                    if let Some(f) = it["for"].as_str()
                        && f != caller.name
                        && caller.profile != Some(f)
                    {
                        return Err(format!(
                            "board_assigned: {thread} is for {f}; pick another from open"
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
                                    "board_waiting: {thread} needs {} done first",
                                    undone.join(", ")
                                ));
                            }
                        }
                        ("claimed", Some(_)) if holds => {
                            return Err(format!("board_held: {thread} is yours already"));
                        }
                        ("claimed", Some(holder)) if !gone(holder, holder_id) => {
                            return Err(format!(
                                "board_held: {holder} holds {thread}; pick another from open"
                            ));
                        }
                        ("claimed", Some(holder)) => {
                            post["took_from"] = json!(holder);
                            if let Some(id) = holder_id {
                                post["took_from_id"] = json!(id);
                            }
                        }
                        (state, _) => {
                            return Err(format!("board_closed: {thread} is {state}"));
                        }
                    }
                }
                "done" | "fail" | "release" => {
                    if state != "claimed" || !holds {
                        return Err(format!(
                            "board_not_holder: only {thread}'s holder {action}s it{}",
                            holder.map_or(String::new(), |h| format!(", {h}"))
                        ));
                    }
                    let opened = match action {
                        "done" => opened_by(dir, board, thread),
                        "release" => vec![thread.to_owned()],
                        _ => Vec::new(),
                    };
                    if !opened.is_empty() {
                        post["opened"] = json!(opened);
                    }
                }
                "cancel" => {
                    if it["by"].as_str() != Some(caller.name) {
                        return Err(format!(
                            "board_not_yours: only {thread}'s maker, {}, cancels it",
                            it["by"].as_str().unwrap_or_default()
                        ));
                    }
                    if state != "open" {
                        return Err(format!(
                            "board_closed: {thread} is {state}; cancel only one nobody holds"
                        ));
                    }
                }
                "reopen" => {
                    if !ours(board) {
                        return Err(not_ours(board, "reopen items"));
                    }
                    if state != "failed" {
                        return Err(format!(
                            "board_invalid: {thread} is {state}; reopen only a failed item"
                        ));
                    }
                    post["opened"] = json!([thread]);
                }
                "label" => {
                    if !ours(board) {
                        return Err(not_ours(board, "label items"));
                    }
                    let labels = labels(&given["labels"], true)?;
                    if labels.is_empty() {
                        return Err("board_invalid: label takes labels to set or remove".into());
                    }
                    post["labels"] = Value::Object(labels);
                }
                "assign" => {
                    if !ours(board) {
                        return Err(not_ours(board, "assign items"));
                    }
                    if !matches!(state, "open" | "claimed") {
                        return Err(format!("board_closed: {thread} is {state}"));
                    }
                    post["for"] = json!(text("for").trim());
                }
                _ => unreachable!("an action is one of ACTIONS"),
            }
        }
    }
    let mut said = commons::add_locked(dir, thread, &post, caller.reader)?;
    if said["posted"] != false {
        let mut after = posts;
        after.push(post);
        said["item"] = item(&after).unwrap_or_default();
    }
    Ok(said)
}

/// Whether the item `need` is done.
fn need_done(dir: &Path, need: &str) -> bool {
    commons::valid_name(need).is_ok()
        && commons::posts(&dir.join(format!("{need}.jsonl")))
            .ok()
            .and_then(|posts| item(&posts))
            .is_some_and(|item| item["state"] == "done")
}

/// The items on `board` that `thread`, done now, opens: open ones that
/// need it, whose other needs are done.
fn opened_by(dir: &Path, board: &str, thread: &str) -> Vec<String> {
    let items = items(dir);
    let done = |need: &str| {
        need == thread
            || (items.iter()).any(|(name, item)| name == need && item["item"]["state"] == "done")
    };
    (items.iter())
        .filter(|(_, item)| {
            let item = &item["item"];
            item["board"] == board
                && item["state"] == "open"
                && (item["needs"].as_array().into_iter().flatten()).any(|n| n == thread)
                && !waiting(item, &done)
        })
        .map(|(name, _)| name.clone())
        .collect()
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

    /// `call` for `name` of `project` made as `profile`, shown every post
    /// of the thread it acts on first, as one that read it is.
    fn send(
        home: &Path,
        name: &str,
        project: &str,
        profile: Option<&str>,
        gone: bool,
        arguments: Value,
    ) -> Result<Value, String> {
        if let Some(thread) = arguments["act"]["thread"].as_str()
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
        };
        call(home, &caller, &arguments, &|_, _| gone)
    }

    fn add(name: &str, title: &str, extra: Value) -> Value {
        let mut add = json!({"name": name, "title": title, "text": "x", "board": "shop/errors"});
        for (key, value) in extra.as_object().into_iter().flatten() {
            add[key] = value.clone();
        }
        json!({"add": add})
    }

    fn act(thread: &str, what: &str) -> Value {
        json!({"act": {"thread": thread, "do": what, "text": "y"}})
    }

    fn open_names(home: &Path) -> Vec<String> {
        let open = send(
            home,
            "x.y",
            "x",
            None,
            false,
            json!({"open": "shop/errors"}),
        )
        .unwrap();
        (open["threads"].as_array().into_iter().flatten())
            .map(|t| t["thread"].as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn an_item_opens_when_its_needs_are_done_and_its_states_are_posts() {
        let home = home("needs");
        let lead = |args| send(&home, "shop.lead", "shop", None, false, args);
        lead(add("store", "Type the store errors", json!({}))).unwrap();
        let api = lead(add("api", "Type the api errors", json!({}))).unwrap();
        assert_eq!(api["item"]["state"], "open");
        lead(add(
            "enum",
            "Remove the old error enum",
            json!({"needs": ["store", "api"]}),
        ))
        .unwrap();
        // Waiting on two: not open, and no claim takes it.
        let mut open = open_names(&home);
        open.sort();
        assert_eq!(open, ["api", "store"]);
        let worker = |args| send(&home, "shop.w1", "shop", None, false, args);
        assert!(
            worker(act("enum", "claim"))
                .unwrap_err()
                .starts_with("board_waiting")
        );
        let claimed = worker(act("store", "claim")).unwrap();
        assert_eq!(claimed["item"]["state"], "claimed");
        assert_eq!(claimed["item"]["holder"], "shop.w1");
        assert_eq!(open_names(&home), ["api"]);
        // Another's claim on a held item is refused while its holder works.
        let other = |args| send(&home, "shop.w2", "shop", None, false, args);
        assert!(
            other(act("store", "claim"))
                .unwrap_err()
                .starts_with("board_held")
        );
        // Only the holder closes it.
        assert!(
            other(act("store", "done"))
                .unwrap_err()
                .starts_with("board_not_holder")
        );
        let done = worker(act("store", "done")).unwrap();
        assert_eq!(done["item"]["state"], "done");
        assert_eq!(done["item"]["result"], "y");
        other(act("api", "claim")).unwrap();
        // The last need done opens the enum item, and its post says so.
        other(act("api", "done")).unwrap();
        let posts = commons::posts(&commons::dir(&home).join("api.jsonl")).unwrap();
        assert_eq!(posts.last().unwrap()["opened"], json!(["enum"]));
        assert_eq!(open_names(&home), ["enum"]);
        // Every step is a post in the thread, nothing rewritten.
        let posts = commons::posts(&commons::dir(&home).join("store.jsonl")).unwrap();
        let steps: Vec<_> = posts.iter().map(|p| p["do"].as_str().unwrap()).collect();
        assert_eq!(steps, ["add", "claim", "done"]);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn only_the_boards_project_takes_its_items_and_anyone_posts() {
        let home = home("project");
        send(
            &home,
            "shop.lead",
            "shop",
            None,
            false,
            add("store", "Type the store errors", json!({})),
        )
        .unwrap();
        let docs = |args| send(&home, "docs.lead", "docs", None, false, args);
        assert!(
            docs(act("store", "claim"))
                .unwrap_err()
                .starts_with("board_not_yours")
        );
        assert!(
            docs(act("store", "label"))
                .unwrap_err()
                .starts_with("board_not_yours")
        );
        // A comment through the commons tool lands with no board fields.
        commons::told(&home, "docs.lead", "store", &[0]);
        let said = commons::call(
            &home,
            "docs.lead",
            None,
            "docs.lead",
            None,
            Some("docs"),
            &json!({"reply": {"thread": "store", "text": "Saw this in docs too"}}),
        )
        .unwrap();
        assert_eq!(said["posts"], 2);
        assert_eq!(open_names(&home), ["store"]);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_claim_takes_over_from_a_closed_holder_and_says_so() {
        let home = home("takeover");
        send(
            &home,
            "shop.lead",
            "shop",
            None,
            false,
            add("store", "Type the store errors", json!({})),
        )
        .unwrap();
        send(&home, "shop.w1", "shop", None, false, act("store", "claim")).unwrap();
        let took = send(&home, "shop.w2", "shop", None, true, act("store", "claim")).unwrap();
        assert_eq!(took["item"]["holder"], "shop.w2");
        let posts = commons::posts(&commons::dir(&home).join("store.jsonl")).unwrap();
        assert_eq!(posts.last().unwrap()["took_from"], "shop.w1");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn an_assigned_item_is_for_its_agent_or_profile_alone() {
        let home = home("assigned");
        let lead = |args| send(&home, "shop.lead", "shop", None, false, args);
        lead(add(
            "review",
            "Review the change",
            json!({"for": "reviewer"}),
        ))
        .unwrap();
        let worker = send(
            &home,
            "shop.w1",
            "shop",
            Some("worker"),
            false,
            act("review", "claim"),
        );
        assert!(worker.unwrap_err().starts_with("board_assigned"));
        send(
            &home,
            "shop.r",
            "shop",
            Some("reviewer"),
            false,
            act("review", "claim"),
        )
        .unwrap();
        // Assign changes who may; empty is anyone.
        lead(add("docs", "Write the docs", json!({}))).unwrap();
        let mut assign = act("docs", "assign");
        assign["act"]["for"] = json!("shop.w9");
        assert_eq!(lead(assign).unwrap()["item"]["for"], "shop.w9");
        let refused = send(&home, "shop.w1", "shop", None, false, act("docs", "claim"));
        assert!(refused.unwrap_err().starts_with("board_assigned"));
        let mut anyone = act("docs", "assign");
        anyone["act"]["for"] = json!("");
        assert!(lead(anyone).unwrap()["item"]["for"].is_null());
        send(&home, "shop.w1", "shop", None, false, act("docs", "claim")).unwrap();
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn titles_are_short_and_never_twice_on_a_board() {
        let home = home("titles");
        let lead = |args| send(&home, "shop.lead", "shop", None, false, args);
        let long = lead(add(
            "long",
            "Type every error in the store and the api",
            json!({}),
        ));
        assert!(long.unwrap_err().starts_with("board_invalid"));
        lead(add("store", "Type the store errors", json!({}))).unwrap();
        let twice = lead(add("store-2", "type the store errors", json!({})));
        assert!(twice.unwrap_err().starts_with("board_exists"));
        // Done, the same title is new work again.
        send(&home, "shop.w1", "shop", None, false, act("store", "claim")).unwrap();
        send(&home, "shop.w1", "shop", None, false, act("store", "done")).unwrap();
        lead(add("store-2", "Type the store errors", json!({}))).unwrap();
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn labels_say_what_an_item_is_and_never_where_it_stands() {
        let home = home("labels");
        let lead = |args| send(&home, "shop.lead", "shop", None, false, args);
        let state = lead(add(
            "bug",
            "Fix the redirect",
            json!({"labels": {"state": "x"}}),
        ));
        assert!(state.unwrap_err().starts_with("board_invalid"));
        lead(add(
            "bug",
            "Fix the redirect",
            json!({"labels": {"kind": "bug", "area": "web"}}),
        ))
        .unwrap();
        lead(add(
            "feature",
            "Add dark mode",
            json!({"labels": {"kind": "feature"}}),
        ))
        .unwrap();
        let bugs = send(
            &home,
            "x.y",
            "x",
            None,
            false,
            json!({"open": "shop/errors", "filter": {"labels": {"kind": ["bug"]}}}),
        )
        .unwrap();
        assert_eq!(bugs["threads"].as_array().unwrap().len(), 1);
        assert_eq!(bugs["threads"][0]["thread"], "bug");
        let mut label = act("bug", "label");
        label["act"]["labels"] = json!({"area": null, "size": "s"});
        let item = lead(label).unwrap()["item"].clone();
        assert_eq!(item["labels"], json!({"kind": "bug", "size": "s"}));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_thread_already_there_goes_on_a_board_and_cancel_is_its_makers() {
        let home = home("item");
        commons::call(
            &home,
            "shop.lead",
            None,
            "shop.lead",
            None,
            Some("shop"),
            &json!({"post": {"name": "flaky", "title": "Fix the flaky test", "text": "x"}}),
        )
        .unwrap();
        let w1 = |args| send(&home, "shop.w1", "shop", None, false, args);
        assert!(
            w1(act("flaky", "claim"))
                .unwrap_err()
                .starts_with("board_not_item")
        );
        let mut item = act("flaky", "item");
        item["act"]["board"] = json!("shop/errors");
        assert_eq!(w1(item.clone()).unwrap()["item"]["state"], "open");
        assert!(w1(item).unwrap_err().starts_with("board_exists"));
        // Its maker, w1, cancels it; the thread's starter does not.
        let lead = send(
            &home,
            "shop.lead",
            "shop",
            None,
            false,
            act("flaky", "cancel"),
        );
        assert!(lead.unwrap_err().starts_with("board_not_yours"));
        assert_eq!(
            w1(act("flaky", "cancel")).unwrap()["item"]["state"],
            "canceled"
        );
        assert!(
            w1(act("flaky", "claim"))
                .unwrap_err()
                .starts_with("board_closed")
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_failed_item_reopens_and_a_released_one_is_open_again() {
        let home = home("reopen");
        let w1 = |args| send(&home, "shop.w1", "shop", None, false, args);
        w1(add("store", "Type the store errors", json!({}))).unwrap();
        w1(act("store", "claim")).unwrap();
        assert_eq!(
            w1(act("store", "release")).unwrap()["item"]["state"],
            "open"
        );
        w1(act("store", "claim")).unwrap();
        assert_eq!(w1(act("store", "fail")).unwrap()["item"]["state"], "failed");
        assert!(open_names(&home).is_empty());
        assert_eq!(w1(act("store", "reopen")).unwrap()["item"]["state"], "open");
        assert_eq!(open_names(&home), ["store"]);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn an_act_after_posts_its_agent_missed_waits_once_as_a_reply_does() {
        let home = home("missed");
        send(
            &home,
            "shop.lead",
            "shop",
            None,
            false,
            add("store", "Type the store errors", json!({})),
        )
        .unwrap();
        let caller = Caller {
            name: "shop.w1",
            id: None,
            reader: "shop.w1",
            model: None,
            project: Some("shop"),
            profile: None,
        };
        // Never shown the item's post: held once, then taken.
        let held = call(&home, &caller, &act("store", "claim"), &|_, _| false).unwrap();
        assert_eq!(held["posted"], false);
        let taken = call(&home, &caller, &act("store", "claim"), &|_, _| false).unwrap();
        assert_eq!(taken["item"]["holder"], "shop.w1");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn the_listing_shows_where_an_item_stands_and_filters_on_it() {
        let home = home("listing");
        let lead = |args| send(&home, "shop.lead", "shop", None, false, args);
        lead(add("store", "Type the store errors", json!({}))).unwrap();
        lead(add(
            "enum",
            "Remove the old enum",
            json!({"needs": ["store"]}),
        ))
        .unwrap();
        let listed = commons::call(
            &home,
            "x.y",
            None,
            "x.y",
            None,
            None,
            &json!({"filter": {"state": ["waiting"]}}),
        )
        .unwrap();
        assert_eq!(listed["threads"].as_array().unwrap().len(), 1);
        assert_eq!(listed["threads"][0]["item"]["state"], "waiting");
        // The summary keeps the item as its posts make it.
        let summary = commons::summary("enum", &commons::dir(&home).join("enum.jsonl")).unwrap();
        assert_eq!(summary["item"]["state"], "open");
        assert_eq!(summary["item"]["needs"], json!(["store"]));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_text_saying_do_is_no_action() {
        let posts = [json!({"by": "a", "at": 1, "title": "T", "text": "\"do\":\"claim\""})];
        assert!(item(&posts).is_none());
        let line = posts[0].to_string();
        assert!(!line.contains(r#""do":"#));
    }

    #[test]
    fn news_names_an_items_board_and_what_it_opened() {
        let home = home("news");
        let cursor = home.join("cursor");
        let _ = commons::news(&home, &cursor).mark_read();
        send(
            &home,
            "shop.lead",
            "shop",
            None,
            false,
            add("store", "Type the store errors", json!({})),
        )
        .unwrap();
        let news = commons::news(&home, &cursor);
        let line: Value = serde_json::from_str(&news.lines[0]).unwrap();
        assert_eq!(line["board"], "shop/errors");
        assert_eq!(line["do"], "add");
        assert_eq!(line["opened"], json!(["store"]));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_board_is_a_projects_path() {
        assert!(valid_board("shop").is_ok());
        assert!(valid_board("shop.web/typed-errors").is_ok());
        for bad in ["", "Shop", "shop//x", "shop/", "../x", "shop/x y"] {
            assert!(valid_board(bad).is_err(), "{bad}");
        }
        assert_eq!(project_of("shop.web/typed-errors"), "shop.web");
    }
}
