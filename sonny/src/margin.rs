//! The margin beside the conversation: notes a bot keeps there with the
//! `margin` tool Sonny declares on it, each pinned to the point in the
//! conversation where it was first put. A marker where a note was put
//! scrolls the margin to it, and the note's ↑ scrolls the conversation
//! back there.
//!
//! The calls are the record, in the bot's history; the margin as it
//! stands is kept beside the person's reactions, by the bot's key, so a
//! window opened later shows it without reading the whole history.
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use crate::live::FRESH;

use ratatui::{
    style::{Color, Stylize},
    text::{Line, Span},
};
use serde_json::{Value, json};
use unicode_width::UnicodeWidthStr;

use crate::screen::{BUTTON, Mark, Tag};
use crate::transcript::Markdown;

/// The name of the tool a bot keeps notes in the margin with.
pub const TOOL: &str = "margin";
/// The most notes a margin holds, as `[sonny] margin_notes` sets it,
/// and the most lines a note's Markdown runs to, as `margin_note_lines`
/// does: a call past either is refused, and says why.
#[cfg(not(test))]
static MOST: [std::sync::atomic::AtomicUsize; 2] = [
    std::sync::atomic::AtomicUsize::new(agent_client::config::MARGIN_NOTES as usize),
    std::sync::atomic::AtomicUsize::new(agent_client::config::MARGIN_NOTE_LINES as usize),
];

// Each test's own, so one that sets them changes no other running beside it.
#[cfg(test)]
thread_local! {
    static MOST: [std::cell::Cell<usize>; 2] = const {
        [
            std::cell::Cell::new(agent_client::config::MARGIN_NOTES as usize),
            std::cell::Cell::new(agent_client::config::MARGIN_NOTE_LINES as usize),
        ]
    };
}

fn most(which: usize) -> usize {
    #[cfg(not(test))]
    return MOST[which].load(std::sync::atomic::Ordering::Relaxed);
    #[cfg(test)]
    return MOST.with(|most| most[which].get());
}

fn set_most(which: usize, to: u64) {
    let to = usize::try_from(to).unwrap_or(usize::MAX).max(1);
    #[cfg(not(test))]
    MOST[which].store(to, std::sync::atomic::Ordering::Relaxed);
    #[cfg(test)]
    MOST.with(|most| most[which].set(to));
}

/// The most notes a margin holds; [`set_most`] sets it.
pub fn most_notes() -> usize {
    most(0)
}

/// The most lines a note's Markdown runs to.
pub fn most_lines() -> usize {
    most(1)
}

/// The margin's limits, as `[sonny] margin_notes` and
/// `margin_note_lines` say: each at least one.
pub fn set_limits(notes: u64, lines: u64) {
    set_most(0, notes);
    set_most(1, lines);
}
/// The most of a note's Markdown kept, and of a file read for one.
const NOTE_BYTES: usize = 64 * 1024;
/// The most lines of a file a note shows.
const FILE_LINES: usize = 400;
/// What a note's rows start with, under its title.
const BODY: &str = "     ";
/// The most rows a note is drawn in, so all the notes together stay
/// under what a [`crate::screen::Screen`] keeps.
const NOTE_ROWS: usize = 600;
/// How many of the calls and messages applied are remembered, so one
/// read again as the conversation is drawn again is not applied twice.
const SEEN: usize = 1024;

/// What the tool says to the bot: when a note earns its place, and kept
/// up to date. A bot a lead makes takes the lead's tools as declared, so
/// what only a lead needs to hear is in its role, not here.
const DESCRIPTION: &str = "Keep something beside the conversation, in the margin the person sees next to your \
         replies. Post a note whenever your answer has a part the person will look at more than \
         once: a diagram of how code or a system fits together (such as in Mermaid), a table of \
         results you measure or compare, or the file being discussed. Put that part in the note, \
         and keep the reply to what it means, with a line pointing to the note, without walking \
         the note's rows again. When the person asks you to walk through a file, show that file in \
         the note and walk through it in the reply; for a walkthrough of anything else, the note \
         holds only what the walkthrough does not, such as a summary table. Skip the margin for a \
         short answer the person reads once, and for \
         the steps of your work, which belong in your plan. Posting the same id again \
         replaces that note in place: update one note as results come in, rather than posting \
         another. When you or a task change what a note shows, such as code a diagram describes, \
         post it again with the same id, rather than a second note about the change, or remove it \
         in that same turn, even while the change waits \
         on a task's branch, saying in the note which it shows, and remove a note once it \
         no longer holds. Where whether a note is still current matters, such as a pull \
         request's state or a check you ran, say when it was last checked, in the person's \
         time zone, and change that as you check again; a note whose content does not age, \
         such as a design, needs no date. Give markdown (rendered as your replies are) or file (a path in your own folder, shown as the file \
         stands and kept fresh as it changes; for a file in a task's copy, put its content in \
         markdown instead, since that copy goes away with the task), or from (a task's note as \
         \"task/id\", as your task updates name it, shown here as the task keeps it and kept once \
         the task is gone, rather than copied). Never ask a question or for a decision only in a \
         note: ask it where the person answers, and the note may point to it, with opens naming \
         the agent that asked it when that is not you.";

/// The `margin` tool as Sonny declares it on a bot.
pub fn tool() -> Value {
    json!({
        "name": TOOL,
        "description": DESCRIPTION,
        "parameters": {
            "type": "object",
            "properties": {
                "id": {"type": "string", "pattern": "^[a-z0-9][a-z0-9-]{0,39}$",
                    "description": "The note's short name, such as routing or repro."},
                "title": {"type": "string", "minLength": 1, "maxLength": 80,
                    "description": "A few words, shown on the note."},
                "markdown": {"type": "string", "maxLength": NOTE_BYTES,
                    "description": "The note, as Markdown."},
                "live": {"type": "boolean",
                    "description": "true: markdown has Jinja blanks Sonny keeps filled: source('name').latest/.topics, file('path'), tasks(), task('name'), now(), |ago, |time, |fromjson."},
                "file": {"type": "string", "minLength": 1,
                    "description": "Or a file to show, absolute or from your folder."},
                "from": {"type": "string", "pattern": "^[A-Za-z0-9._-]{1,128}/[a-z0-9][a-z0-9-]{0,39}$",
                    "description": "Or a task's note to show, as \"task/id\"."},
                "opens": {"type": "string", "pattern": "^[A-Za-z0-9._-]{0,128}$",
                    "description": "An agent the note is about, by its name, such as a task whose question waits on the person: the note gets a button that opens it beside the conversation. Empty takes the button away."},
                "remove": {"type": "boolean", "description": "true takes the note away."}
            },
            "required": ["id"],
            "additionalProperties": false
        },
        "answered_by": crate::served::TAG
    })
}

/// What a `margin` call is answered: "In the margin.", or refused, typed,
/// as [`refusal`] says, by the margin of `bot` as its history stands. It
/// is read on from where it was last read here, kept under its key in
/// `store` apart from a window's, so each call reads only what came
/// since. The calls
/// read are as they were answered: one answered then stays, whatever the
/// limits are now.
pub async fn answer(
    client: &agent_client::Client,
    store: &Path,
    bot: &str,
    (call_id, arguments): (&str, &Value),
) -> Result<String, (&'static str, String)> {
    let unread = |e: agent_client::Error| ("margin_unread", e.to_string());
    // As the person's config says now; one that does not read, the defaults.
    let sonny = agent_client::config::load()
        .map(|config| config.sonny)
        .unwrap_or_default();
    set_limits(sonny.margin_notes, sonny.margin_note_lines);
    let page = (client
        .request("bots", json!({"name": bot, "limit": 1}))
        .await)
        .map_err(unread)?;
    let record = (page["bots"].get(0)).filter(|record| record["name"] == bot);
    let id = (record.and_then(|record| record["bot_id"].as_i64()))
        .ok_or(("margin_unread", format!("{bot} is gone")))?;
    let workspace = (record.and_then(|record| record["workspace"].as_str()))
        .map(PathBuf::from)
        .unwrap_or_default();
    let store = std::fs::canonicalize(store).unwrap_or_else(|_| store.to_owned());
    let key = crate::reactions::key(&store, id);
    // Its own, never a window's: a window reads past calls still waiting
    // on their answers, which this one reads up to.
    let mut margin = Margin::load(Some(&answers(&key)));
    let mut after = margin.through().unwrap_or(0);
    // Calls not answered yet, by id: where each was made, in what order,
    // and what it asked.
    let mut calls: HashMap<String, (i64, usize, Value)> = HashMap::new();
    let mut made = 0;
    loop {
        let asked = json!({"bot": bot, "limit": 400, "min_node": after + 1, "oldest_first": true});
        let page = client
            .request("history_nodes", asked)
            .await
            .map_err(unread)?;
        let mut nodes: Vec<i64> = (page["nodes"].as_array().into_iter().flatten())
            .filter_map(|node| node["node"].as_i64())
            .collect();
        nodes.sort_unstable();
        let Some(&last) = nodes.last() else {
            break;
        };
        let (items, short) = crate::needs::items(client, bot, &nodes).await;
        if short {
            return Err(("margin_unread", format!("{bot}'s history was read short")));
        }
        for (node, item) in items {
            let call = item["call_id"].as_str().unwrap_or("").to_owned();
            match (item["type"].as_str(), item["role"].as_str()) {
                (_, Some("user")) => {
                    if let Some((closed, _)) = changed(&item) {
                        margin.remove(&closed, Some((node, &format!("m{node}"))));
                    }
                }
                (Some("function_call"), _) if item["name"] == TOOL => {
                    let arguments = (item["arguments"].as_str())
                        .and_then(|a| serde_json::from_str(a).ok())
                        .unwrap_or(Value::Null);
                    calls.insert(call, (node, made, arguments));
                    made += 1;
                }
                (Some("function_call_output"), _) => {
                    if let Some((_, _, arguments)) = calls.remove(&call)
                        && answered(&item)
                    {
                        margin.apply(&call, &arguments, None, None);
                    }
                }
                _ => {}
            }
        }
        after = last;
    }
    // Read through to the first call still waiting on its answer, this
    // one among them: the next read takes it up again with its answer.
    let through = calls
        .values()
        .map(|(node, ..)| node - 1)
        .fold(after, i64::min);
    if through > 0 {
        margin.advance(through);
    }
    // Calls made before this one and not answered yet, as when one
    // response makes several, are answered as this one is, in the order
    // they were made: each taken if it fits, so calls made together never
    // hold more than the margin takes. A removal makes room only once its
    // answer is in, since it may yet fail. Kept nowhere: their answers do.
    let mine = calls.get(call_id).map(|(_, made, _)| *made);
    let mut before: Vec<(&String, &(i64, usize, Value))> = (calls.iter())
        .filter(|(id, (_, made, _))| *id != call_id && mine.is_none_or(|mine| *made < mine))
        .collect();
    before.sort_by_key(|(_, (_, made, _))| *made);
    let mut held = Margin {
        kept: margin.kept.clone(),
        ..Margin::default()
    };
    for (id, (.., arguments)) in before {
        if arguments["remove"] != true && held.refusal(arguments).is_none() {
            held.apply(id, arguments, None, None);
        }
    }
    if let Some((code, why)) = held.refusal(arguments) {
        return Err((code, why));
    }
    match live(arguments) {
        Some(template) if shown_as_template(arguments) => {
            let data = crate::template::Data {
                store: Some(store),
                // The ones the person set up, as the window has them: the
                // commons keeps nothing a blank reads.
                sources: crate::template::configured_sources(),
                folder: workspace,
                now: SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs() as i64),
                // The host does not read where the tasks stand: a blank
                // that names one is filled in the window.
                tasks: Default::default(),
            };
            filled(template, &data)
        }
        _ => Ok("In the margin.".into()),
    }
}

/// The template a call gives: its `markdown`, when `live` is true.
fn live(arguments: &Value) -> Option<&str> {
    (arguments["live"] == true)
        .then(|| arguments["markdown"].as_str())
        .flatten()
}

/// Whether a call's note shows its live `markdown`: no file or task's
/// note named over it.
fn shown_as_template(arguments: &Value) -> bool {
    live(arguments).is_some()
        && arguments["from"].as_str().and_then(from).is_none()
        && !(arguments["file"].as_str()).is_some_and(|file| !file.trim().is_empty())
}

/// What a call posting live `template` is answered, filled from `data` as the
/// tool host has it: refused when it does not read as a template, names a
/// source the person has not set up, or fills to more lines than a note
/// holds; else what it shows now, or why it does not fill yet, as a blank
/// that names a task the window fills.
fn filled(template: &str, data: &crate::template::Data) -> Result<String, (&'static str, String)> {
    // Whole or not at all: a template cut short would read as another.
    if template.len() > NOTE_BYTES {
        return Err((
            "margin_note_too_long",
            format!(
                "the template is {} bytes and a margin note holds at most {NOTE_BYTES}: \
                 keep what it says short and let its blanks fill the rest",
                template.len()
            ),
        ));
    }
    crate::template::refusal(template, data, crate::template::To::Window)
        .map_err(|why| (crate::template::REFUSED, why))?;
    match crate::template::render(template, data).0 {
        Err(why) => Ok(format!("In the margin; it does not fill here yet ({why}).")),
        Ok(text) if text.lines().count() > most_lines() => Err((
            "margin_note_too_long",
            format!(
                "the template fills to {} lines and a margin note holds at most {}: \
                 show less of what it reads, such as the newest few events",
                text.lines().count(),
                most_lines()
            ),
        )),
        Ok(text) => Ok(format!(
            "In the margin, filled as it changes. It shows now:\n{text}"
        )),
    }
}

/// The margin of the bot `id` in `store` started at node `head` of its
/// history, as a side chat's is at its copy: what came before is its
/// source's, and holds none of its own notes.
pub fn start_at(store: &Path, id: i64, head: i64) {
    let store = std::fs::canonicalize(store).unwrap_or_else(|_| store.to_owned());
    Margin::load(Some(&answers(&crate::reactions::key(&store, id)))).advance(head);
}

/// Where [`answer`] keeps the margin of the bot kept under `key` as it
/// was answered: apart from the window's, which only a window writes.
fn answers(key: &str) -> String {
    format!("{key}.answered")
}

/// Whether a call's `output` says it was taken: neither failed nor
/// refused.
fn answered(output: &Value) -> bool {
    output["failed"] != true && !crate::reactions::refused(output)
}

/// What a note shows.
#[derive(Clone, Debug, PartialEq)]
pub enum Body {
    Markdown(String),
    /// Markdown with blanks, filled as what they read changes
    /// ([`crate::template`]).
    Template(String),
    File(String),
    /// A file's text as read where another bot keeps it, shown as the
    /// file is: only what a [`Body::From`] shows.
    Read {
        path: String,
        text: String,
    },
    /// Another bot's note `id`, as it was last read there: kept once that
    /// bot or its note is gone.
    From {
        bot: String,
        id: String,
        /// Its title there, as this note last took it.
        title: Option<String>,
        shown: Option<Box<Body>>,
        /// The folder what it shows was read from there, for its pictures.
        folder: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Note {
    pub id: String,
    pub title: String,
    pub body: Body,
    /// The call that first put it here, where it is pinned.
    pub call: String,
    /// The person's message the bot was answering then, from which the
    /// conversation is drawn again to jump back to the note.
    pub after: Option<i64>,
    /// The turn that last changed it after it was put there.
    pub turn: Option<i64>,
    /// The agent its button opens beside the conversation.
    pub opens: Option<String>,
}

/// What a call did to the margin.
#[derive(Debug, PartialEq)]
pub enum Change {
    /// A note new to the margin, by its id.
    Posted(String),
    Updated(String),
    /// A note taken away, by its title.
    Removed(String),
}

#[derive(Clone, Default)]
struct Kept {
    notes: Vec<Note>,
    /// The calls and messages applied, newest last.
    seen: VecDeque<String>,
    /// The person closed the margin: it stays closed until they open it.
    closed: bool,
    /// The newest node of the bot's history the notes were brought up to:
    /// a window replays only the `margin` calls and closes after it.
    through: Option<i64>,
    /// What another bot's template note last filled to here, by note id:
    /// kept, as a file read there is, once that bot's folder is gone.
    filled: BTreeMap<String, String>,
}

impl Kept {
    fn json(&self) -> Value {
        let notes: Vec<Value> = (self.notes.iter())
            .map(|note| {
                let mut json = json!({"id": note.id, "title": note.title, "call": note.call,
                    "after": note.after, "turn": note.turn, "opens": note.opens});
                body_json(&note.body, &mut json);
                json
            })
            .collect();
        json!({"notes": notes, "seen": self.seen, "closed": self.closed, "through": self.through,
            "filled": self.filled})
    }

    /// What [`Kept::json`] wrote; a note it cannot read is left out.
    fn read(json: &Value) -> Self {
        let text = |note: &Value, key: &str| note[key].as_str().map(str::to_owned);
        let notes = (json["notes"].as_array().into_iter().flatten())
            .filter_map(|note| {
                let body = body_read(note)?;
                Some(Note {
                    id: text(note, "id")?,
                    title: text(note, "title")?,
                    body,
                    call: text(note, "call")?,
                    after: note["after"].as_i64(),
                    turn: note["turn"].as_i64(),
                    opens: text(note, "opens"),
                })
            })
            .collect();
        let seen = (json["seen"].as_array().into_iter().flatten())
            .filter_map(|seen| seen.as_str().map(str::to_owned))
            .collect();
        Kept {
            notes,
            seen,
            closed: json["closed"] == true,
            through: json["through"].as_i64(),
            filled: (json["filled"].as_object().into_iter().flatten())
                .filter_map(|(id, text)| Some((id.clone(), text.as_str()?.to_owned())))
                .collect(),
        }
    }
}

/// A note's words as a reply quotes them: its text, a file's path, or
/// what another bot's note showed when last read.
fn said(body: &Body) -> String {
    match body {
        Body::Markdown(text) | Body::Template(text) | Body::Read { text, .. } => text.clone(),
        Body::File(path) => path.clone(),
        Body::From { shown, .. } => shown.as_deref().map(said).unwrap_or_default(),
    }
}

/// `body` written into a kept note's `json`.
fn body_json(body: &Body, json: &mut Value) {
    match body {
        Body::Markdown(text) => json["markdown"] = json!(text),
        Body::Template(text) => {
            json["markdown"] = json!(text);
            json["live"] = json!(true);
        }
        Body::File(path) => json["file"] = json!(path),
        Body::Read { path, text } => {
            json["read"] = json!(path);
            json["text"] = json!(text);
        }
        Body::From {
            bot,
            id,
            title,
            shown,
            folder,
        } => {
            json["from"] = json!(format!("{bot}/{id}"));
            json["shown_title"] = json!(title);
            json["shown_folder"] = json!(folder);
            if let Some(shown) = shown {
                let mut kept = json!({});
                body_json(shown, &mut kept);
                json["shown"] = kept;
            }
        }
    }
}

/// What [`body_json`] wrote.
fn body_read(json: &Value) -> Option<Body> {
    let text = |key: &str| json[key].as_str().map(str::to_owned);
    if let Some((bot, id)) = json["from"].as_str().and_then(from) {
        let shown = body_read(&json["shown"])
            .filter(|shown| !matches!(shown, Body::From { .. }))
            .map(Box::new);
        return Some(Body::From {
            bot,
            id,
            title: text("shown_title"),
            shown,
            folder: text("shown_folder"),
        });
    }
    if let Some(path) = text("read") {
        return Some(Body::Read {
            path,
            text: text("text").unwrap_or_default(),
        });
    }
    match text("file") {
        Some(path) => Some(Body::File(path)),
        None if json["live"] == true => Some(Body::Template(text("markdown")?)),
        None => Some(Body::Markdown(text("markdown")?)),
    }
}

/// The note a bot's history `item` changes in its margin, and how: a
/// `margin` call its margin takes, or the person's message taking one
/// away, read as a call that does.
fn changed(item: &Value) -> Option<(String, Value)> {
    if item["role"] == "user" {
        let said = match &item["content"] {
            Value::Array(parts) => parts.iter().find_map(|part| part["text"].as_str()),
            content => content.as_str(),
        };
        let id = closed(said?)?;
        return Some((id.to_owned(), json!({"id": id, "remove": true})));
    }
    if item["type"] != "function_call" || item["name"] != TOOL {
        return None;
    }
    let arguments = (item["arguments"].as_str())
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .filter(fits)?;
    let id = arguments["id"].as_str()?.trim().to_owned();
    Some((id, arguments))
}

/// Why a margin holding the notes `held`, by id, refuses the call
/// `arguments`, typed,
/// and in words for the agent: a new note once it holds the most
/// notes, or Markdown longer than a note's most lines, counted in the
/// text given rather than the rows drawn, so a window's width changes
/// nothing. Taking a note away, and a note that shows a file or a
/// task's note, are never refused for their length. Notes kept from
/// before a lower limit stay; only new ones wait for room.
pub fn refusal(held: &[&str], arguments: &Value) -> Option<(&'static str, String)> {
    if arguments["remove"] == true {
        return None;
    }
    let id = arguments["id"].as_str().map(str::trim).unwrap_or("");
    // As the note will show it: a file or a task's note only when named;
    // a template's lines are counted as it fills, by the tool host.
    let shows = arguments["from"].as_str().and_then(from).is_some()
        || arguments["file"]
            .as_str()
            .is_some_and(|file| !file.trim().is_empty())
        || live(arguments).is_some();
    let lines = (arguments["markdown"].as_str())
        .filter(|_| !shows)
        .map_or(0, |markdown| markdown.lines().count());
    if lines > most_lines() {
        return Some((
            "margin_note_too_long",
            format!(
                "the note is {lines} lines and a margin note holds at most {}: keep it to \
                 what the person looks at more than once, or write the longer text to a \
                 file and show that file in the note",
                most_lines()
            ),
        ));
    }
    if held.len() >= most_notes() && !held.contains(&id) {
        return Some((
            "margin_full",
            format!(
                "the margin holds {} notes and takes at most {}: {}. Post one of those \
                 ids again to change that note, or remove one that no longer holds",
                held.len(),
                most_notes(),
                held.join(", ")
            ),
        ));
    }
    None
}

/// The bot and the note a `from` names, as `bot/id`.
pub fn from(text: &str) -> Option<(String, String)> {
    let (bot, id) = text.trim().rsplit_once('/')?;
    (is_agent(bot) && is_id(id)).then(|| (bot.to_owned(), id.to_owned()))
}

/// Whether `name` reads as an agent's name.
fn is_agent(name: &str) -> bool {
    let named = |c: char| c.is_ascii_alphanumeric() || "._-".contains(c);
    !name.is_empty() && name.len() <= 128 && name.chars().all(named)
}

/// Another bot's note, as read from its history: the newest node read,
/// the bot read, the folder its files are read from, whether the bot is gone, and the
/// note as last posted there with its file's text, when it shows one.
#[derive(Default)]
pub struct Reference {
    pub node: Option<i64>,
    /// The bot's identity, which a later bot given its name does not share.
    pub bot_id: Option<i64>,
    pub workspace: Option<String>,
    pub gone: bool,
    title: String,
    /// Whether it was given its title there, not its id for want of one:
    /// untitled, one it shows of another bot's is titled as that one.
    titled: bool,
    body: Option<Body>,
    file: Option<String>,
    /// Taken away there since it was last posted.
    removed: bool,
    /// Its margin calls for the note read but not yet answered there, by
    /// id: one changes what is shown once its answer says it was taken.
    waiting: HashMap<String, Value>,
    /// The note it shows of another bot's, when it shows one, as read
    /// from there: one step on, no further.
    pub through: Option<Box<Reference>>,
    /// The bot and note it was read from, when read through another.
    pub names: Option<(String, String)>,
}

impl Reference {
    /// The bot's `items` read, oldest first: its margin calls for note
    /// `id` change what is shown once their answers there say they were
    /// taken, and the person taking it away there does at once.
    pub fn read(&mut self, id: &str, items: &[Value]) {
        for item in items {
            if item["type"] == "function_call_output" {
                let call = item["call_id"].as_str().unwrap_or("");
                if let Some(arguments) = self.waiting.remove(call)
                    && answered(item)
                {
                    self.take(id, id, &arguments);
                }
                continue;
            }
            let Some((note, arguments)) = changed(item) else {
                continue;
            };
            match item["type"] == "function_call" {
                // Only its own: calls for other notes change nothing here.
                true if note == id => {
                    let call = item["call_id"].as_str().unwrap_or("").to_owned();
                    self.waiting.insert(call, arguments);
                }
                true => {}
                false => self.take(id, &note, &arguments),
            }
        }
    }

    /// The change to `note` that `arguments` made there, taken: only note
    /// `id` changes what is shown. Taken away, by the bot or the person,
    /// what was read stays, and one put there again starts untitled.
    fn take(&mut self, id: &str, note: &str, arguments: &Value) {
        if note != id {
            return;
        }
        if arguments["remove"] == true {
            self.removed = true;
            return;
        }
        // Kept, as there, when changed without one.
        match arguments["title"].as_str().map(str::trim) {
            Some(title) if !title.is_empty() => {
                self.title = title.to_owned();
                self.titled = true;
            }
            _ if self.title.is_empty() || self.removed => {
                self.title = id.to_owned();
                self.titled = false;
            }
            _ => {}
        }
        let recreated = std::mem::take(&mut self.removed);
        let body = match (
            arguments["from"].as_str().and_then(from),
            arguments["file"].as_str(),
            live(arguments),
            arguments["markdown"].as_str(),
        ) {
            (Some((bot, id)), ..) => Body::From {
                bot,
                id,
                title: None,
                shown: None,
                folder: None,
            },
            (_, Some(file), ..) if !file.trim().is_empty() => Body::File(file.trim().to_owned()),
            (_, _, Some(template), _) => Body::Template(cut(template, NOTE_BYTES).to_owned()),
            (.., Some(markdown)) => Body::Markdown(cut(markdown, NOTE_BYTES).to_owned()),
            // Put there again with nothing in it, as its own margin does.
            _ if recreated => Body::Markdown(String::new()),
            // Retitled only.
            _ => return,
        };
        if !matches!((&self.body, &body), (Some(Body::File(a)), Body::File(b)) if a == b) {
            self.file = None;
        }
        self.body = Some(body);
    }

    /// Whether the bot's `items` set the note `id`'s text and title at
    /// once, or take it away: what is shown from there on can be read
    /// without older ones.
    pub fn starts(id: &str, items: &[Value]) -> bool {
        let taken: Vec<&str> = (items.iter())
            .filter(|item| item["type"] == "function_call_output" && answered(item))
            .filter_map(|item| item["call_id"].as_str())
            .collect();
        let taken = |item: &&Value| {
            item["type"] != "function_call"
                || taken.contains(&item["call_id"].as_str().unwrap_or(""))
        };
        items
            .iter()
            .filter(taken)
            .filter_map(changed)
            .any(|(at, arguments)| {
                at == id
                    && (arguments["remove"] == true
                        || arguments["title"]
                            .as_str()
                            .is_some_and(|t| !t.trim().is_empty())
                            && (arguments["markdown"].is_string()
                                || arguments["from"].as_str().and_then(from).is_some()
                                || arguments["file"]
                                    .as_str()
                                    .is_some_and(|f| !f.trim().is_empty())))
            })
    }

    /// Whether the note is taken away there: what was read of it stays.
    pub fn removed(&self) -> bool {
        self.removed
    }

    /// The other bot's note this one shows, by bot and id.
    pub fn shows(&self) -> Option<(String, String)> {
        match &self.body {
            Some(Body::From { bot, id, .. }) => Some((bot.clone(), id.clone())),
            _ => None,
        }
    }

    /// The file the note shows, its path in the bot's folder: none once
    /// the note is taken away there, what was read of it kept as it was.
    pub fn file(&self) -> Option<PathBuf> {
        let Some(Body::File(path)) = &self.body.as_ref().filter(|_| !self.removed) else {
            return None;
        };
        Some(Path::new(self.workspace.as_deref()?).join(path))
    }

    /// The file's `text` as read now, as [`read`] bounds it: kept once it
    /// can no longer be.
    pub fn file_read(&mut self, text: Option<String>) {
        if let Some(text) = text {
            self.file = Some(text);
        }
    }

    /// The note's title there, what it shows, and the folder that is
    /// read from: a file as the text read of it, so that it stays once
    /// the bot's folder is gone.
    pub fn shown(&self) -> Option<(&str, Body, Option<PathBuf>)> {
        let workspace = self.workspace.as_deref().map(PathBuf::from);
        let body = match self.body.as_ref()? {
            // As read there, under this one's title, or that one's when
            // it has none of its own.
            Body::From { .. } => {
                let (title, body, folder) = self.through.as_ref()?.shown()?;
                let title = if self.titled { &self.title } else { title };
                return Some((title, body, folder));
            }
            // Not shown until read: what was kept of it stays.
            Body::File(path) => {
                let folder = (workspace.map(|w| w.join(path)))
                    .and_then(|file| file.parent().map(Path::to_path_buf));
                let body = Body::Read {
                    path: path.clone(),
                    text: self.file.clone()?,
                };
                return Some((&self.title, body, folder));
            }
            body => body.clone(),
        };
        Some((&self.title, body, workspace))
    }
}

/// A file a note shows, as last read: its time, when it was looked at,
/// and its text.
struct Read {
    modified: Option<SystemTime>,
    looked: Instant,
    text: Result<String, String>,
}

/// What a margin's template notes are filled from, as the window has it
/// ([`crate::template`]), and the person's own notes.
#[derive(Clone, Default)]
pub struct Blanks {
    /// The store whose lookout keeps the sources' events.
    pub store: Option<PathBuf>,
    /// The sources the person set up, by name.
    pub sources: Arc<Vec<String>>,
    /// The agents as `/status` last read them, as `tasks()` gives them.
    pub tasks: Arc<Vec<Value>>,
    /// Counts each change to `tasks`.
    pub tasks_read: u64,
    /// The person's own notes, `[sonny.notes.NAME]`, shown over the
    /// agent's: each its name, title and Markdown, live or not.
    pub notes: Arc<Vec<(String, String, Body)>>,
}

/// What a margin's rows were drawn from: its width, the running turn,
/// the terminal's light, how many times its notes and files changed,
/// and the folder its files are read from.
type Drawn = (usize, Option<i64>, bool, u64, PathBuf);

/// The rows a margin's notes draw, with their marks.
type Rows = (Vec<Line<'static>>, Vec<(usize, Mark)>);

/// The notes in a bot's margin.
#[derive(Default)]
pub struct Margin {
    file: Option<PathBuf>,
    kept: Kept,
    reads: HashMap<PathBuf, Read>,
    /// Counts each change to the notes or the files they show.
    changes: u64,
    drawn: Option<(Drawn, Rows)>,
    /// Counts each note put up or changed by the bot.
    posted: u64,
    /// The bot's plan as its newest `plan` call set it, shown over the
    /// notes: one, however often it changes.
    plan: Vec<String>,
    /// What template notes are filled from, as the window sets it before
    /// each draw ([`Margin::set_blanks`]).
    blanks: Blanks,
    /// Template notes as last filled, by note id; the person's own by
    /// `~NAME`.
    watch: crate::live::Watch,
}

/// The number a note's pin goes by on the screen: its call's, the same
/// on every run.
pub fn anchor(call: &str) -> u64 {
    call.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

impl Margin {
    /// The margin kept under `key`, the bot's [`crate::reactions::key`];
    /// none kept, or no key, is an empty one.
    pub fn load(key: Option<&str>) -> Self {
        let file = key.and_then(|key| {
            let home = std::env::var_os("HOME")?;
            Some(PathBuf::from(home).join(format!(".agent/sonny/margin/{key}.json")))
        });
        let kept = (file.as_ref())
            .and_then(|file| std::fs::read(file).ok())
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .map(|json| Kept::read(&json))
            .unwrap_or_default();
        Margin {
            file,
            kept,
            ..Margin::default()
        }
    }

    /// The notes that show another bot's: each note's id, the bot, and
    /// the note there.
    pub fn references(&self) -> Vec<(String, String, String)> {
        (self.kept.notes.iter())
            .filter_map(|note| match &note.body {
                Body::From { bot, id, .. } => Some((note.id.clone(), bot.clone(), id.clone())),
                _ => None,
            })
            .collect()
    }

    /// The note `note` shows of another bot's, read there as `body`,
    /// with that note's `title` when this one was given none.
    pub fn resolved(&mut self, note: &str, title: &str, body: Body, read_in: Option<PathBuf>) {
        let Some(at) = self.kept.notes.iter().position(|n| n.id == note) else {
            return;
        };
        let note = &mut self.kept.notes[at];
        let Body::From {
            title: took,
            shown,
            folder,
            ..
        } = &mut note.body
        else {
            return;
        };
        let read_in = read_in.map(|folder| folder.display().to_string());
        // Titled as there for as long as it was given no title of its own.
        let inherited = note.title == note.id || Some(&note.title) == took.as_ref();
        let retitled = inherited && !title.is_empty() && note.title != title;
        if shown.as_deref() == Some(&body) && !retitled && *folder == read_in {
            return;
        }
        *shown = Some(Box::new(body));
        *folder = read_in;
        if retitled {
            note.title = title.to_owned();
            *took = Some(title.to_owned());
        }
        self.save();
    }

    /// The plan shown over the notes now `steps`, none taking it away.
    pub fn set_plan(&mut self, steps: &[String]) {
        if self.plan != steps {
            self.plan = steps.to_vec();
            self.changes += 1;
        }
    }

    pub fn notes(&self) -> &[Note] {
        &self.kept.notes
    }

    pub fn note(&self, id: &str) -> Option<&Note> {
        self.kept.notes.iter().find(|note| note.id == id)
    }

    /// A note as a reply quotes it: its title, then what it says when the
    /// agent wrote it out, or the file it shows.
    pub fn quoted(&self, id: &str) -> Option<String> {
        let note = self.note(id)?;
        Some(
            format!("✎ {}\n{}", note.title, said(&note.body))
                .trim_end()
                .to_owned(),
        )
    }

    /// Whether it holds no note and no plan.
    pub fn is_empty(&self) -> bool {
        self.kept.notes.is_empty() && self.plan.is_empty() && self.blanks.notes.is_empty()
    }

    /// The newest node of the bot's history the notes were brought up to.
    pub fn through(&self) -> Option<i64> {
        self.kept.through
    }

    /// Whether what the history holds at `node` is in the notes already.
    pub fn holds(&self, node: i64) -> bool {
        self.kept.through.is_some_and(|through| node <= through)
    }

    /// The notes brought up to `node` of the bot's history.
    pub fn advance(&mut self, node: i64) {
        if !self.holds(node) {
            self.kept.through = Some(node);
            self.save();
        }
    }

    /// Whether the person closed the margin.
    pub fn closed(&self) -> bool {
        self.kept.closed
    }

    /// The margin closed, or open again, as the person asked.
    pub fn close(&mut self, closed: bool) {
        if self.kept.closed != closed {
            self.kept.closed = closed;
            self.save();
        }
    }

    /// Whether `what`, a call's id or a message's, was applied already;
    /// it is counted as applied now.
    fn seen(&mut self, what: &str) -> bool {
        if self.kept.seen.iter().any(|seen| seen == what) {
            return true;
        }
        self.kept.seen.push_back(what.to_owned());
        while self.kept.seen.len() > SEEN {
            self.kept.seen.pop_front();
        }
        false
    }

    /// The `margin` call `call` with `arguments` applied, made while the
    /// bot answered the message at `after`, in `turn`; nothing for a call
    /// applied before or one that says nothing to do.
    pub fn apply(
        &mut self,
        call: &str,
        arguments: &Value,
        after: Option<i64>,
        turn: Option<i64>,
    ) -> Option<Change> {
        if !fits(arguments) {
            return None;
        }
        let id = arguments["id"]
            .as_str()
            .map(str::trim)
            .filter(|id| is_id(id))?;
        if self.seen(call) {
            return None;
        }
        let change = self.change(id, call, arguments, after, turn);
        if matches!(change, Some(Change::Posted(_) | Change::Updated(_))) {
            self.posted += 1;
        }
        self.save();
        change
    }

    /// Counts each note the bot put up or changed while this margin was
    /// read: a different count, something new to show.
    pub fn posted(&self) -> u64 {
        self.posted
    }

    /// Why the margin as it stands refuses the call `arguments`: see
    /// [`refusal`].
    pub fn refusal(&self, arguments: &Value) -> Option<(&'static str, String)> {
        let held: Vec<&str> = self
            .kept
            .notes
            .iter()
            .map(|note| note.id.as_str())
            .collect();
        refusal(&held, arguments)
    }

    fn change(
        &mut self,
        id: &str,
        call: &str,
        arguments: &Value,
        after: Option<i64>,
        turn: Option<i64>,
    ) -> Option<Change> {
        let at = self.kept.notes.iter().position(|note| note.id == id);
        if arguments["remove"] == true {
            let note = self.kept.notes.remove(at?);
            return Some(Change::Removed(note.title));
        }
        let title = arguments["title"]
            .as_str()
            .map(str::trim)
            .filter(|t| !t.is_empty());
        // An agent's name, or none for an empty one; unsaid, as it was.
        let opens = arguments["opens"]
            .as_str()
            .map(str::trim)
            .map(|name| Some(name.to_owned()).filter(|name| is_agent(name)));
        let had = at.map(|at| &self.kept.notes[at].body);
        let body = match (
            arguments["from"].as_str().and_then(from),
            arguments["file"].as_str(),
            live(arguments),
            arguments["markdown"].as_str(),
        ) {
            // The same note again keeps what was read of it.
            (Some((bot, id)), ..) => Some(match had {
                Some(Body::From {
                    bot: b,
                    id: i,
                    title,
                    shown,
                    folder,
                }) if *b == bot && *i == id => Body::From {
                    bot,
                    id,
                    title: title.clone(),
                    shown: shown.clone(),
                    folder: folder.clone(),
                },
                _ => Body::From {
                    bot,
                    id,
                    title: None,
                    shown: None,
                    folder: None,
                },
            }),
            (_, Some(file), ..) if !file.trim().is_empty() => {
                Some(Body::File(file.trim().to_owned()))
            }
            (_, _, Some(template), _) => Some(Body::Template(cut(template, NOTE_BYTES).to_owned())),
            (.., Some(markdown)) => Some(Body::Markdown(cut(markdown, NOTE_BYTES).to_owned())),
            _ => None,
        };
        match at {
            Some(at) => {
                let note = &mut self.kept.notes[at];
                if let Some(title) = title {
                    note.title = title.to_owned();
                }
                if let Some(body) = body {
                    note.body = body;
                }
                if let Some(opens) = opens {
                    note.opens = opens;
                }
                note.turn = turn;
                Some(Change::Updated(id.to_owned()))
            }
            None => {
                self.kept.notes.push(Note {
                    id: id.to_owned(),
                    title: title.unwrap_or(id).to_owned(),
                    body: body.unwrap_or(Body::Markdown(String::new())),
                    call: call.to_owned(),
                    after,
                    turn: None,
                    opens: opens.flatten(),
                });
                Some(Change::Posted(id.to_owned()))
            }
        }
    }

    /// The note `id` taken away by the person: from here as they ask
    /// (`None`), or as the message at `node`, read with the conversation,
    /// told the bot, which the key `seen` says was applied; its title. A
    /// note put there in answer to a later message is a new one, and stays.
    pub fn remove(&mut self, id: &str, read: Option<(i64, &str)>) -> Option<String> {
        if let Some((node, seen)) = read
            && (self.seen(seen) || self.note(id).is_some_and(|note| note.after > Some(node)))
        {
            return None;
        }
        let at = self.kept.notes.iter().position(|note| note.id == id);
        let title = at.map(|at| self.kept.notes.remove(at).title);
        self.save();
        title
    }

    /// A message of the person's that took a note away, by its `key`,
    /// counted as applied: the note went when they asked.
    pub fn applied(&mut self, key: &str) {
        self.seen(key);
        self.save();
    }

    fn save(&mut self) {
        self.changes += 1;
        let Some(file) = &self.file else {
            return;
        };
        // Kept for the next window; a margin not kept is still shown here.
        let _ = (|| -> std::io::Result<()> {
            if let Some(dir) = file.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let json = serde_json::to_vec(&self.kept.json()).map_err(std::io::Error::other)?;
            let next = file.with_extension("json.next");
            std::fs::write(&next, json)?;
            std::fs::rename(&next, file)
        })();
    }

    /// The margin's notes as rows `width` wide, files read from
    /// `workspace`, those the running turn `live` changes marked so:
    /// each note's first row tagged with its pin and carrying its
    /// buttons, ↑ back to where it was put and × to take it away. The
    /// rows last drawn while nothing they show changed.
    pub fn lines(&mut self, width: usize, workspace: &Path, live: Option<i64>) -> Rows {
        self.freshen(workspace);
        let from = (
            width,
            live,
            crate::term::light(),
            self.changes,
            workspace.to_owned(),
        );
        if let Some((drawn, rows)) = &self.drawn
            && *drawn == from
        {
            return rows.clone();
        }
        let rows = self.draw(width, workspace, live);
        self.drawn = Some((from, rows.clone()));
        rows
    }

    /// The files notes show looked at again, so one changed counts as a
    /// change; only those are kept read.
    /// What template notes are filled from now. The person's own notes
    /// differing, by one added, taken away or retitled, is a change.
    pub fn set_blanks(&mut self, blanks: Blanks) {
        if !Arc::ptr_eq(&self.blanks.notes, &blanks.notes) && self.blanks.notes != blanks.notes {
            self.changes += 1;
        }
        self.blanks = blanks;
    }

    pub fn freshen(&mut self, workspace: &Path) {
        let files: Vec<PathBuf> = (self.kept.notes.iter())
            .filter_map(|note| match &note.body {
                Body::File(path) => Some(workspace.join(path)),
                Body::Markdown(_) | Body::Template(_) | Body::Read { .. } | Body::From { .. } => {
                    None
                }
            })
            .collect();
        self.reads.retain(|path, _| files.contains(path));
        for path in &files {
            self.look(path);
        }
        // Templates, the person's own first: another bot's is filled
        // here, its files read where that bot keeps them. Only those due
        // a look are copied out, so a frame between looks costs nothing.
        let person = (self.blanks.notes.iter()).filter_map(|(name, _, body)| match body {
            Body::Template(template) => Some((format!("~{name}"), template, None, false)),
            _ => None,
        });
        let notes = (self.kept.notes.iter()).filter_map(|note| match &note.body {
            Body::Template(template) => Some((note.id.clone(), template, None, false)),
            Body::From {
                shown: Some(shown),
                folder,
                ..
            } => match shown.as_ref() {
                Body::Template(template) => {
                    Some((note.id.clone(), template, folder.as_deref(), true))
                }
                _ => None,
            },
            _ => None,
        });
        let mut keys = Vec::new();
        let mut due = Vec::new();
        for (key, template, folder, kept) in person.chain(notes) {
            let folder = folder.map_or_else(|| workspace.to_owned(), PathBuf::from);
            if self.watch.due(&key, template, &folder) {
                due.push((key.clone(), template.clone(), folder, kept));
            }
            keys.push(key);
        }
        if (self.watch).retain(|key| keys.iter().any(|k| k == key)) {
            self.changes += 1;
        }
        let was = self.kept.filled.len();
        (self.kept.filled).retain(|key, _| keys.contains(key));
        if self.kept.filled.len() != was {
            self.save();
        }
        for (key, template, folder, kept) in due {
            self.fill(&key, &template, &folder, kept);
        }
    }

    /// The template note `key` filled again from `folder` once what it
    /// read last changed ([`crate::live::Watch::fill`]). A fill that
    /// shows something new counts as a change. Another bot's (`kept`)
    /// keeps what it filled to, so a later fill that fails there, its
    /// folder gone, still shows it, in this window and the next.
    fn fill(&mut self, key: &str, template: &str, folder: &Path, kept: bool) {
        let last = kept.then(|| self.kept.filled.get(key).cloned()).flatten();
        if !(self.watch).fill(key, (template, folder), &self.blanks, last) {
            return;
        }
        self.changes += 1;
        if kept
            && let Some(filled) = self.watch.get(key)
            && filled.error.is_none()
            && let Some(text) = &filled.text
            && self.kept.filled.get(key) != Some(text)
        {
            self.kept.filled.insert(key.to_owned(), text.clone());
            self.save();
        }
    }

    /// The template note `key`'s rows as last filled, `width` wide, its
    /// pictures read from `folder`: its text cut at the most lines a note
    /// holds, then why its last fill failed, when it did.
    fn filled_lines(&self, key: &str, width: usize, folder: &Path) -> Vec<Line<'static>> {
        crate::live::lines(self.watch.get(key), width, folder, most_lines(), BODY)
    }

    /// How many times what the margin shows has changed: a note, a file a
    /// note shows, or the margin shown or closed.
    pub fn changes(&self) -> u64 {
        self.changes
    }

    /// The note `id`'s rows `width` wide, files read from `workspace`, cut
    /// at `most` rows with a row saying how many more there are.
    pub fn body(
        &mut self,
        id: &str,
        width: usize,
        workspace: &Path,
        most: usize,
    ) -> Option<Vec<Line<'static>>> {
        let note = self.note(id)?.body.clone();
        // Another bot's, its pictures read where it keeps them.
        let (shown, folder) = match &note {
            Body::From {
                shown: Some(shown),
                folder,
                ..
            } => (shown.as_ref(), folder.as_deref().map(Path::new)),
            body => (body, None),
        };
        let mut body = match shown {
            Body::Markdown(text) => {
                let mut markdown = Markdown::in_folder(folder.unwrap_or(workspace));
                crate::transcript::markdown(&mut markdown, text, width, leads())
            }
            Body::Template(_) => self.filled_lines(id, width, folder.unwrap_or(workspace)),
            Body::File(path) => self.file_lines(workspace, path, width),
            Body::Read { path, text } => {
                text_lines(Path::new(path), text, folder.unwrap_or(workspace), width)
            }
            Body::From { bot, id, .. } => {
                let line = format!("{BODY}{bot}'s note {id}, once it is read");
                vec![Line::from(line).dim()]
            }
        };
        if body.len() > most {
            let more = body.len() - most;
            body.truncate(most);
            body.push(Line::from(format!("{BODY}  … {more} more rows")).dim());
        }
        Some(body)
    }

    fn draw(&mut self, width: usize, workspace: &Path, live: Option<i64>) -> Rows {
        let mut lines = Vec::new();
        let mut marks = Vec::new();
        if !self.plan.is_empty() {
            lines.push(Line::default());
            let done = self
                .plan
                .iter()
                .filter(|step| step.starts_with("[x]"))
                .count();
            // ≡, one column in every terminal: ☰ is two since Unicode 16
            // and one in most terminals still, which left a letter of what
            // was drawn before beside it.
            lines.push(Line::from(vec![
                Span::raw(" ≡ ").fg(Color::Cyan),
                Span::raw("Plan").bold(),
                Span::raw(format!("  {done}/{}", self.plan.len())).dim(),
            ]));
            for step in crate::calls::plan(&self.plan).under {
                let lead = || Span::raw(BODY);
                lines.extend(crate::transcript::wrap(step, width, lead(), lead()));
            }
        }
        // The person's own, over the agent's: theirs to change in their
        // config, so no buttons.
        let person = self.blanks.notes.clone();
        for (name, title, body) in person.iter() {
            lines.push(Line::default());
            let title = crate::transcript::cut(title, width.saturating_sub(4));
            lines.push(Line::from(vec![
                Span::raw(" ◇ ").dim(),
                Span::raw(title).bold(),
            ]));
            match body {
                Body::Template(_) => {
                    lines.extend(self.filled_lines(&format!("~{name}"), width, workspace));
                }
                body => {
                    let mut markdown = Markdown::in_folder(workspace);
                    let text = said(body);
                    lines.extend(crate::transcript::markdown(
                        &mut markdown,
                        &text,
                        width,
                        leads(),
                    ));
                }
            }
        }
        if self.kept.notes.is_empty() && (!self.plan.is_empty() || !person.is_empty()) {
            return (lines, marks);
        }
        if self.kept.notes.is_empty() {
            lines.push(Line::default());
            lines.push(
                Line::from(" Notes the agent keeps beside the conversation show here.").dim(),
            );
            return (lines, marks);
        }
        let notes = self.kept.notes.clone();
        for note in &notes {
            lines.push(Line::default());
            let at = lines.len();
            let (head, buttons) = head(note, width, live.is_some() && note.turn == live);
            lines.push(head);
            marks.push((at, Mark::Tag(Tag::Note(anchor(&note.call)))));
            marks.push((at, Mark::Buttons(buttons)));
            lines.extend(
                self.body(&note.id, width, workspace, NOTE_ROWS)
                    .unwrap_or_default(),
            );
        }
        (lines, marks)
    }

    /// The file at `path` read again once it changes, looked at no more
    /// than once a [`FRESH`].
    fn look(&mut self, path: &Path) {
        let now = Instant::now();
        if (self.reads.get(path)).is_some_and(|read| now.duration_since(read.looked) < FRESH) {
            return;
        }
        let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        match self.reads.get_mut(path) {
            Some(read) if modified.is_some() && read.modified == modified => read.looked = now,
            _ => {
                let text = read(path, &[]);
                let read = Read {
                    modified,
                    looked: now,
                    text,
                };
                self.reads.insert(path.to_owned(), read);
                self.changes += 1;
            }
        }
    }

    /// The file `path` names, from `workspace`, as a note shows it:
    /// Markdown as replies read, anything else as code.
    fn file_lines(&mut self, workspace: &Path, path: &str, width: usize) -> Vec<Line<'static>> {
        let path = workspace.join(path);
        self.look(&path);
        let read = &self.reads[&path];
        let text = match &read.text {
            Ok(text) => text,
            Err(error) => return vec![Line::from(format!("{BODY}{error}")).dim()],
        };
        // Its pictures read from beside it.
        let folder = path.parent().unwrap_or(workspace).to_owned();
        text_lines(&path, text, &folder, width)
    }
}

/// A note's rows' leads, as the shared Markdown renderer takes them.
fn leads() -> (Line<'static>, Line<'static>) {
    (Line::from(BODY), Line::from(BODY))
}

/// The file at `path`, as much of it as a note shows, refused when what
/// was opened is in one of `hidden` ([`agent_client::hidden::holds`]).
pub fn read(path: &Path, hidden: &[PathBuf]) -> Result<String, String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    // A pipe or a device could hold the window up: only files are read.
    match std::fs::metadata(path) {
        Ok(meta) if !meta.is_file() => return Err(format!("{}: not a file", path.display())),
        _ => {}
    }
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if agent_client::hidden::holds(hidden, &file) {
        return Err(format!(
            "read_denied: {} is kept unreadable from agents",
            path.display()
        ));
    }
    file.take(NOTE_BYTES as u64 * 4)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    match String::from_utf8(bytes) {
        Ok(text) if !text.contains('\0') => Ok(text),
        Err(error) if error.utf8_error().error_len().is_none() => {
            let valid = error.utf8_error().valid_up_to();
            let mut bytes = error.into_bytes();
            bytes.truncate(valid);
            Ok(String::from_utf8(bytes).unwrap_or_default())
        }
        _ => Err(format!("{} is not text", path.display())),
    }
}

/// `text` cut to at most `most` bytes, on a character's edge.
fn cut(text: &str, most: usize) -> &str {
    let mut end = text.len().min(most);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// A note's first row: its title, `live` while the running turn changes
/// it, and its buttons on the right; and where those buttons are.
fn head(note: &Note, width: usize, live: bool) -> (Line<'static>, crate::screen::Buttons) {
    let right = match note.opens {
        Some(_) => "↗  ↩  ↑  ×",
        None => "↩  ↑  ×",
    };
    let tail = match live {
        true => " live",
        false => "",
    };
    let room = width.saturating_sub(right.width() + tail.width() + 4);
    let title = crate::transcript::cut(&note.title, room);
    let used = 3 + title.width() + tail.width();
    let gap = width.saturating_sub(used + right.width()).max(1);
    let opens = (used + gap) as u16;
    let mut spans = vec![
        Span::raw(" ✎ ").dim(),
        Span::raw(title).bold(),
        Span::raw(tail).fg(Color::Cyan),
        Span::raw(" ".repeat(gap)),
    ];
    let id = &note.id;
    let mut buttons = Vec::new();
    // The agent it is about opens beside the conversation, as a click on
    // that agent's row does.
    if let Some(agent) = &note.opens {
        spans.extend([Span::raw("↗").fg(Color::Cyan), Span::raw("  ")]);
        buttons.push((opens, opens + 1, format!("{BUTTON}task/{agent}")));
    }
    // ↩ answers it, quoted where the person writes, as a message's own
    // mark does.
    let reply = opens + 3 * u16::from(note.opens.is_some());
    let up = reply + 3;
    spans.extend([
        Span::raw(crate::screen::REPLY).dim(),
        Span::raw("  "),
        Span::raw("↑").dim(),
        Span::raw("  "),
        Span::raw("×").dim(),
    ]);
    buttons.extend([
        (0, opens.saturating_sub(1), format!("{BUTTON}jump/{id}")),
        (up, up + 1, format!("{BUTTON}jump/{id}")),
        (up + 3, up + 4, format!("{BUTTON}close/{id}")),
        (reply, reply + 1, format!("{BUTTON}reply-note/{id}")),
    ]);
    (Line::from(spans), buttons)
}

/// A file's `text` as a note shows the file at `path`: Markdown as
/// replies read, its pictures from `folder`, anything else as code.
fn text_lines(path: &Path, text: &str, folder: &Path, width: usize) -> Vec<Line<'static>> {
    let extension = (path.extension().and_then(|e| e.to_str()))
        .unwrap_or_default()
        .to_lowercase();
    let lines: Vec<&str> = text.lines().collect();
    let shown = lines.len().min(FILE_LINES);
    let mut out = match extension.as_str() {
        "md" | "markdown" | "mdx" => {
            let mut markdown = Markdown::document(folder);
            crate::transcript::markdown(&mut markdown, &lines[..shown].join("\n"), width, leads())
        }
        // Code as code, whatever it holds.
        _ => {
            let mut code = crate::highlight::Code::file(path, lines.first().unwrap_or(&""));
            let lead = || Span::raw(BODY);
            (lines[..shown].iter())
                .flat_map(|text| {
                    let text = text.replace('\t', "    ");
                    let spans = match &mut code {
                        Some(code) => code.line(&text),
                        None => vec![Span::raw(text)],
                    };
                    crate::transcript::wrap(Line::from(spans), width, lead(), lead())
                })
                .collect()
        }
    };
    if lines.len() > shown {
        let more = lines.len() - shown;
        out.push(Line::from(format!("{BODY}… {more} more lines")).dim());
    }
    out
}

/// What the agent is told when the person takes its note `id` away,
/// with the next model call.
pub fn closed_message(id: &str) -> String {
    format!("{CLOSED}{id}\" away.")
}

const CLOSED: &str = "I took your margin note \"";

/// The note a message [`closed_message`] made took away, by its id.
pub fn closed(text: &str) -> Option<&str> {
    let id = text.strip_prefix(CLOSED)?.strip_suffix("\" away.")?;
    is_id(id).then_some(id)
}

/// The note a `margin` call's `tool_started` event names, and whether the
/// call takes it away. The event may carry only the start of a long
/// call's arguments: the id is read from where it shows then.
pub fn called(data: &Value) -> Option<(String, bool)> {
    let text = data["arguments"].as_str()?;
    if let Ok(arguments) = serde_json::from_str::<Value>(text) {
        let id = arguments["id"].as_str().filter(|id| is_id(id))?;
        return Some((id.to_owned(), arguments["remove"] == true));
    }
    let rest = text.split_once("\"id\"")?.1.trim_start();
    let rest = rest.strip_prefix(':')?.trim_start().strip_prefix('"')?;
    let id = rest.split_once('"')?.0;
    is_id(id).then(|| (id.to_owned(), false))
}

/// Whether `arguments` are a call the daemon runs, as it checks one: only
/// the tool's fields, each of its type. Applied as the call starts, a
/// call it would refuse changes nothing.
fn fits(arguments: &Value) -> bool {
    let tool = tool();
    let properties = &tool["parameters"]["properties"];
    arguments.as_object().is_some_and(|fields| {
        fields
            .iter()
            .all(|(field, value)| match properties[field]["type"].as_str() {
                Some("string") => value.is_string(),
                Some("boolean") => value.is_boolean(),
                _ => false,
            })
    })
}

/// Whether `id` is one a note may go by, as the tool's schema says: no
/// more than a short slug, so nothing else the agent wrote is ever said
/// back to it as the person's.
fn is_id(id: &str) -> bool {
    let mut chars = id.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id.len() <= 40
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn margin() -> Margin {
        Margin::default()
    }

    /// The rows drawn, as text.
    fn drawn(margin: &mut Margin, folder: &Path) -> String {
        let (lines, _) = margin.lines(60, folder, None);
        (lines.iter())
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Every template note looked at again as if a [`FRESH`] had passed.
    fn later(margin: &mut Margin) {
        margin.watch.later();
    }

    #[test]
    fn a_template_note_fills_and_fills_again_only_once_what_it_read_changes() {
        let mut margin = margin();
        margin.blanks.tasks = Arc::new(vec![json!({"name": "calc.add", "state": "working"})]);
        let post = json!({"id": "tasks", "title": "Tasks",
            "live": true, "markdown": "{% for t in tasks() %}- {{ t.name }}: {{ t.state }}\n{% endfor %}"});
        assert_eq!(
            margin.apply("c1", &post, None, None),
            Some(Change::Posted("tasks".into()))
        );
        assert_eq!(
            margin.note("tasks").unwrap().body,
            Body::Template(post["markdown"].as_str().unwrap().into())
        );
        let folder = Path::new("/");
        let rows = drawn(&mut margin, folder);
        assert!(rows.contains("follows tasks\n") && rows.contains("calc.add: working"));
        // Kept as the template, never what it filled to.
        let kept = Kept::read(&margin.kept.json());
        assert!(matches!(&kept.notes[0].body, Body::Template(t) if t.contains("tasks()")));
        // A new read of the tasks fills it again; the same read does not.
        let changes = margin.changes();
        later(&mut margin);
        drawn(&mut margin, folder);
        assert_eq!(margin.changes(), changes, "nothing it read changed");
        margin.blanks.tasks = Arc::new(vec![json!({"name": "calc.add", "state": "failed"})]);
        margin.blanks.tasks_read += 1;
        later(&mut margin);
        assert!(drawn(&mut margin, folder).contains("calc.add: failed"));
    }

    #[test]
    fn a_template_note_names_the_source_it_follows_and_says_when_it_is_down() {
        let store = std::env::temp_dir()
            .join(format!("sonny-margin-down-{}", std::process::id()))
            .join("store.sqlite");
        let dir = crate::lookout::beside(&store, ".sources").join(".recent");
        std::fs::create_dir_all(&dir).unwrap();
        let line = json!({"source": "ci", "text": "green", "at_ms": 1000, "seq": 1});
        std::fs::write(dir.join("ci.jsonl"), format!("{line}\n")).unwrap();
        let mut margin = margin();
        margin.blanks.store = Some(store.clone());
        margin.blanks.sources = Arc::new(vec!["ci".into()]);
        let post =
            json!({"id": "ci", "live": true, "markdown": "CI is {{ source('ci').latest.text }}"});
        margin.apply("c1", &post, None, None);
        let rows = drawn(&mut margin, Path::new("/"));
        assert!(
            rows.contains("follows ci\n") && rows.contains("CI is green"),
            "{rows}"
        );
        let down = json!({"source": "ci", "text": "down", "lookout": true, "at_ms": 9000,
            "since_ms": 5000, "id": "lookout.down.9000", "seq": 2});
        std::fs::write(dir.join("ci.jsonl"), format!("{line}\n{down}\n")).unwrap();
        later(&mut margin);
        let rows = drawn(&mut margin, Path::new("/"));
        let since = crate::stamp::when(5);
        assert!(
            rows.contains(&format!("follows ci down since {since}")),
            "{rows}"
        );
        assert!(rows.contains("CI is green"), "{rows}");
        // The source taken out of the config: the note fills again and
        // says so, though its log did not change.
        margin.set_blanks(Blanks {
            store: Some(store.clone()),
            ..Default::default()
        });
        later(&mut margin);
        let rows = drawn(&mut margin, Path::new("/"));
        assert!(rows.contains("no source named ci"), "{rows}");
        let _ = std::fs::remove_dir_all(store.parent().unwrap());
    }

    #[test]
    fn a_tasks_template_note_keeps_what_it_filled_to_once_its_folder_is_gone() {
        let dir = std::env::temp_dir().join(format!("sonny-margin-task-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("state.json"), r#"{"state": "open"}"#).unwrap();
        let mut margin = margin();
        margin.kept.notes.push(Note {
            id: "pr".into(),
            title: "PR".into(),
            body: Body::From {
                bot: "calc".into(),
                id: "pr".into(),
                title: None,
                shown: Some(Box::new(Body::Template(
                    "PR is {{ (file('state.json') | fromjson).state }}".into(),
                ))),
                folder: Some(dir.display().to_string()),
            },
            call: "c1".into(),
            after: None,
            turn: None,
            opens: None,
        });
        assert!(drawn(&mut margin, Path::new("/")).contains("PR is open"));
        // The task removed, then a new window: what it showed stays.
        std::fs::remove_dir_all(&dir).unwrap();
        let mut again = Margin {
            kept: Kept::read(&margin.kept.json()),
            ..Margin::default()
        };
        let rows = drawn(&mut again, Path::new("/"));
        assert!(rows.contains("PR is open") && rows.contains("⟨"), "{rows}");
        // Taken away, it keeps nothing.
        again.kept.notes.clear();
        drawn(&mut again, Path::new("/"));
        assert!(again.kept.filled.is_empty());
    }

    #[test]
    fn a_template_that_fails_later_keeps_what_it_showed_and_says_why() {
        let dir =
            std::env::temp_dir().join(format!("sonny-margin-template-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("state.json"), r#"{"state": "open"}"#).unwrap();
        let mut margin = margin();
        let post = json!({"id": "pr", "live": true, "markdown": "PR is {{ (file('state.json') | fromjson).state }}"});
        margin.apply("c1", &post, None, None);
        let rows = drawn(&mut margin, &dir);
        assert!(rows.contains("follows state.json") && rows.contains("PR is open"));
        std::fs::write(dir.join("state.json"), "not json").unwrap();
        later(&mut margin);
        let rows = drawn(&mut margin, &dir);
        assert!(
            rows.contains("PR is open") && rows.contains("⟨line 1: not JSON"),
            "{rows}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_persons_own_notes_show_over_the_agents_without_buttons() {
        let mut margin = margin();
        margin.blanks.notes = Arc::new(vec![
            (
                "now".into(),
                "Right now".into(),
                Body::Template("{{ tasks() | length }} tasks".into()),
            ),
            (
                "plain".into(),
                "Plain".into(),
                Body::Markdown("{{ as written }}".into()),
            ),
        ]);
        assert!(!margin.is_empty());
        margin.apply(
            "c1",
            &json!({"id": "a", "markdown": "the agent's"}),
            None,
            None,
        );
        let rows = drawn(&mut margin, Path::new("/"));
        let (person, agent) = (
            rows.find("◇ Right now").unwrap(),
            rows.find("the agent's").unwrap(),
        );
        assert!(person < agent && rows.contains("0 tasks"), "{rows}");
        // One not live shows as written: its blanks are words.
        assert!(rows.contains("{{ as written }}"), "{rows}");
        // Retitled, then taken out of the config: the margin shows it so.
        let blanks = |notes: Vec<(String, String, Body)>| Blanks {
            notes: Arc::new(notes),
            ..Default::default()
        };
        let template = Body::Template("{{ tasks() | length }} tasks".to_owned());
        margin.set_blanks(blanks(vec![("now".into(), "Today".into(), template)]));
        let rows = drawn(&mut margin, Path::new("/"));
        assert!(
            rows.contains("◇ Today") && !rows.contains("Right now"),
            "{rows}"
        );
        margin.set_blanks(blanks(Vec::new()));
        let rows = drawn(&mut margin, Path::new("/"));
        assert!(
            !rows.contains("◇") && rows.contains("the agent's"),
            "{rows}"
        );
    }

    #[test]
    fn the_tool_host_refuses_a_template_that_cannot_fill_and_says_what_one_shows() {
        let data = crate::template::Data {
            sources: Arc::new(vec!["ci".into()]),
            ..Default::default()
        };
        let refused = |template: &str| filled(template, &data).unwrap_err().0;
        assert_eq!(refused("{% if %}"), "live_template");
        assert_eq!(refused("{{ source('cl').latest }}"), "live_template");
        let long = "{% for i in range(40) %}{{ i }}\n{% endfor %}";
        assert_eq!(refused(long), "margin_note_too_long");
        // Too big to keep whole: refused, never kept cut.
        let big = format!("{{# {} #}}ok", "x".repeat(NOTE_BYTES));
        assert_eq!(refused(&big), "margin_note_too_long");
        assert_eq!(
            filled("{{ 1 + 1 }} done", &data).unwrap(),
            "In the margin, filled as it changes. It shows now:\n2 done"
        );
        // A task the host cannot see is filled in the window.
        let later = filled("{{ task('calc.add').state }}", &data).unwrap();
        assert!(
            later.starts_with("In the margin; it does not fill here yet"),
            "{later}"
        );
    }

    #[test]
    fn the_plan_is_drawn_in_glyphs_one_column_wide_everywhere() {
        let mut margin = margin();
        margin.set_plan(&["[x] Run the tests".to_owned(), "[ ] Push".to_owned()]);
        let (lines, _) = margin.lines(60, Path::new("/"), None);
        // Each cell is one character: a glyph Unicode counts as two while
        // terminals draw it as one leaves a stale letter beside it.
        for line in &lines {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            assert_eq!(text.width(), text.chars().count(), "{text}");
        }
        assert!(
            lines
                .iter()
                .any(|line| line.spans.iter().any(|s| s.content.contains('≡')))
        );
    }

    #[test]
    fn a_note_is_posted_updated_in_place_and_removed() {
        let mut margin = margin();
        let post = json!({"id": "repro", "title": "Repro", "markdown": "1 of 5"});
        assert_eq!(
            margin.apply("c1", &post, Some(7), Some(3)),
            Some(Change::Posted("repro".into()))
        );
        let update = json!({"id": "repro", "markdown": "2 of 5"});
        assert_eq!(
            margin.apply("c2", &update, Some(9), Some(4)),
            Some(Change::Updated("repro".into()))
        );
        let note = margin.note("repro").unwrap();
        // Still pinned where it was first put, its title kept.
        assert_eq!((note.call.as_str(), note.after), ("c1", Some(7)));
        assert_eq!(note.title, "Repro");
        assert_eq!(note.body, Body::Markdown("2 of 5".into()));
        assert_eq!(note.turn, Some(4));
        let remove = json!({"id": "repro", "remove": true});
        assert_eq!(
            margin.apply("c3", &remove, None, None),
            Some(Change::Removed("Repro".into()))
        );
        assert!(margin.notes().is_empty());
    }

    #[test]
    fn a_margin_holds_the_history_it_was_brought_up_to() {
        let mut margin = margin();
        assert_eq!(margin.through(), None);
        assert!(!margin.holds(1));
        margin.advance(9);
        margin.advance(4);
        assert_eq!(margin.through(), Some(9));
        assert!(margin.holds(9) && !margin.holds(10));
        let again = Kept::read(&margin.kept.json());
        assert_eq!(again.through, Some(9));
    }

    #[test]
    fn a_margin_kept_reads_back_the_same() {
        let mut margin = margin();
        margin.apply(
            "c1",
            &json!({"id": "a", "title": "A", "markdown": "x"}),
            Some(4),
            Some(2),
        );
        margin.apply(
            "c2",
            &json!({"id": "b", "title": "B", "file": "f.md"}),
            None,
            None,
        );
        margin.close(true);
        let again = Kept::read(&margin.kept.json());
        assert_eq!(again.notes, margin.kept.notes);
        assert_eq!(again.seen, margin.kept.seen);
        assert!(again.closed);
    }

    /// `items` read as another bot's history, each margin call there
    /// answered as taken.
    fn answered_read(reference: &mut Reference, id: &str, items: &[Value]) {
        let mut history = Vec::new();
        for (n, item) in items.iter().enumerate() {
            let mut item = item.clone();
            if item["type"] != "function_call" {
                history.push(item);
                continue;
            }
            let call = format!("c{n}");
            item["call_id"] = json!(call);
            history.push(item);
            history.push(json!({"type": "function_call_output", "call_id": call, "output": "In the margin."}));
        }
        reference.read(id, &history);
    }

    #[test]
    fn a_task_s_note_taken_away_by_the_person_starts_again_empty() {
        let call = |arguments: Value| json!({"type": "function_call", "name": TOOL, "arguments": arguments.to_string()});
        let closed = |id: &str| json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": closed_message(id)}]});
        // Taken away by the person there: no longer read, and put there
        // again with a title only, it shows nothing.
        let mut reference = Reference {
            workspace: Some("/work/calc.add".into()),
            ..Default::default()
        };
        answered_read(
            &mut reference,
            "flow",
            &[
                call(json!({"id": "flow", "title": "Flow", "file": "src/flow.rs"})),
                closed("other"),
            ],
        );
        assert!(reference.file().is_some());
        let taken = [closed("flow")];
        assert!(Reference::starts("flow", &taken));
        answered_read(&mut reference, "flow", &taken);
        assert!(reference.removed());
        assert_eq!(reference.file(), None);
        answered_read(
            &mut reference,
            "flow",
            &[call(json!({"id": "flow", "title": "Again"}))],
        );
        let (title, body, _) = reference.shown().unwrap();
        assert_eq!((title, body), ("Again", Body::Markdown(String::new())));
        // Refused there, as its answer says, it changes nothing here,
        // whatever the limits are now; taken, it does.
        let mut reference = Reference::default();
        answered_read(
            &mut reference,
            "flow",
            &[call(
                json!({"id": "flow", "title": "Flow", "markdown": "one"}),
            )],
        );
        let mut asked = call(json!({"id": "flow", "markdown": "two"}));
        asked["call_id"] = json!("refused");
        let refused = json!({"type": "function_call_output", "call_id": "refused", "failed": true,
            "output": json!({"error": "margin_note_too_long", "detail": "-"}).to_string()});
        reference.read("flow", &[asked.clone(), refused]);
        assert_eq!(reference.shown().unwrap().1, Body::Markdown("one".into()));
        // Not answered yet, it waits for its answer, read later.
        asked["call_id"] = json!("later");
        reference.read("flow", &[asked]);
        assert_eq!(reference.shown().unwrap().1, Body::Markdown("one".into()));
        let taken =
            json!({"type": "function_call_output", "call_id": "later", "output": "In the margin."});
        assert!(!Reference::starts("flow", std::slice::from_ref(&taken)));
        reference.read("flow", &[taken]);
        assert_eq!(reference.shown().unwrap().1, Body::Markdown("two".into()));
        // Many made at once, every answer after them: its own is taken.
        let mut asked: Vec<Value> = (0..20)
            .map(|n| {
                let mut other = call(json!({"id": format!("o{n}"), "markdown": "-"}));
                other["call_id"] = json!(format!("o{n}"));
                other
            })
            .collect();
        let mut own = call(json!({"id": "flow", "markdown": "three"}));
        own["call_id"] = json!("own");
        asked.insert(0, own);
        let answers: Vec<Value> = (asked.iter())
            .map(|call| {
                json!({"type": "function_call_output", "call_id": call["call_id"],
                "output": "In the margin."})
            })
            .collect();
        reference.read("flow", &asked);
        reference.read("flow", &answers);
        assert_eq!(reference.shown().unwrap().1, Body::Markdown("three".into()));
    }

    #[test]
    fn a_task_s_untitled_note_showing_another_s_is_titled_as_that_one() {
        let call = |arguments: Value| json!({"type": "function_call", "name": TOOL, "arguments": arguments.to_string()});
        let mut sub = Reference::default();
        answered_read(
            &mut sub,
            "flow",
            &[call(
                json!({"id": "flow", "title": "Sub flow", "markdown": "deep"}),
            )],
        );
        let mut reference = Reference::default();
        answered_read(
            &mut reference,
            "flow",
            &[call(json!({"id": "flow", "from": "calc.sub/flow"}))],
        );
        reference.through = Some(Box::new(sub));
        assert_eq!(reference.shown().unwrap().0, "Sub flow");
        // Given its own, it keeps it.
        answered_read(
            &mut reference,
            "flow",
            &[call(json!({"id": "flow", "title": "Flow"}))],
        );
        assert_eq!(reference.shown().unwrap().0, "Flow");
    }

    #[test]
    fn a_task_s_note_is_shown_as_it_stands_there_and_kept() {
        let mut margin = margin();
        let post = json!({"id": "flow", "from": "calc.add/flow"});
        assert!(fits(&post));
        margin.apply("c1", &post, None, None);
        assert_eq!(
            margin.references(),
            [("flow".into(), "calc.add".into(), "flow".into())]
        );
        // Read there: its calls for that note, oldest first.
        let call = |arguments: Value| json!({"type": "function_call", "name": TOOL, "arguments": arguments.to_string()});
        let mut reference = Reference::default();
        answered_read(
            &mut reference,
            "flow",
            &[
                call(json!({"id": "flow", "title": "Request flow", "markdown": "one"})),
                call(json!({"id": "other", "markdown": "not this"})),
                call(json!({"id": "flow", "markdown": "two"})),
                // Retitled only, it keeps what it shows.
                call(json!({"id": "flow", "title": "Request path"})),
                call(json!({"id": "flow", "remove": true})),
            ],
        );
        // A retitle alone is no start to read back to.
        let retitled = [call(json!({"id": "flow", "title": "Request path"}))];
        assert!(!Reference::starts("flow", &retitled));
        let (title, body, _) = reference.shown().unwrap();
        margin.resolved("flow", title, body, None);
        let note = margin.note("flow").unwrap();
        // Titled as the task titled it, it showing what the task's shows.
        assert_eq!(note.title, "Request path");
        let Body::From { shown, .. } = &note.body else {
            panic!()
        };
        assert_eq!(shown.as_deref(), Some(&Body::Markdown("two".into())));
        // A file, as its text: kept once the task's folder is gone.
        reference.workspace = Some("/work/calc.add".into());
        answered_read(
            &mut reference,
            "flow",
            &[call(json!({"id": "flow", "file": "src/flow.rs"}))],
        );
        assert_eq!(
            reference.file(),
            Some(PathBuf::from("/work/calc.add/src/flow.rs"))
        );
        reference.file_read(Some("fn flow() {}".into()));
        reference.file_read(None);
        // Taken away and put there again untitled with nothing in it, it
        // shows nothing, as there.
        let mut again = Reference::default();
        answered_read(
            &mut again,
            "flow",
            &[
                call(json!({"id": "flow", "title": "Old", "markdown": "old"})),
                call(json!({"id": "flow", "remove": true})),
                call(json!({"id": "flow", "title": "New"})),
            ],
        );
        let (title, body, _) = again.shown().unwrap();
        assert_eq!((title, body), ("New", Body::Markdown(String::new())));
        // Taken away there, the file is no longer read; posted again, it is.
        let removed = [call(json!({"id": "flow", "remove": true}))];
        answered_read(&mut reference, "flow", &removed);
        assert_eq!(reference.file(), None);
        answered_read(
            &mut reference,
            "flow",
            &[call(json!({"id": "flow", "file": "src/flow.rs"}))],
        );
        assert!(reference.file().is_some());
        let (_, body, folder) = reference.shown().unwrap();
        let read = |path: &str, text: &str| Body::Read {
            path: path.into(),
            text: text.into(),
        };
        assert_eq!(body, read("src/flow.rs", "fn flow() {}"));
        let changes = margin.changes();
        assert_eq!(folder, Some(PathBuf::from("/work/calc.add/src")));
        margin.resolved("flow", "Request flow", body.clone(), folder.clone());
        margin.resolved("flow", "Request flow", body, folder);
        assert_eq!(margin.changes(), changes + 1);
        // Shown as the file is there: a page as its source.
        let rows = |margin: &mut Margin| {
            (margin
                .body("flow", 80, Path::new("/nowhere"), 100)
                .unwrap()
                .iter())
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n")
        };
        assert!(rows(&mut margin).contains("fn flow() {}"));
        margin.resolved("flow", "Request flow", read("page.html", "<p>hi</p>"), None);
        assert!(rows(&mut margin).contains("<p>hi</p>"));
        // A reply to it quotes what it shows, as one to a note of its own.
        assert!(margin.quoted("flow").unwrap().ends_with("\n<p>hi</p>"));
        // A file not read yet, here anew, leaves what was kept of it.
        let mut anew = Reference::default();
        anew.read(
            "flow",
            &[
                call(json!({"id": "flow", "title": "Flow", "file": "src/flow.rs"})),
                call(json!({"id": "flow", "remove": true})),
            ],
        );
        assert!(anew.shown().is_none());
        // Posted again, it keeps what was read; and it is kept.
        margin.apply(
            "c2",
            &json!({"id": "flow", "from": "calc.add/flow"}),
            None,
            None,
        );
        let again = Kept::read(&margin.kept.json());
        assert_eq!(again.notes, margin.kept.notes);
        let Body::From { shown: Some(_), .. } = &margin.note("flow").unwrap().body else {
            panic!()
        };
        // Only a bot's note names one.
        for named in ["calc.add", "/flow", "calc.add/Flow", "a b/flow"] {
            assert_eq!(from(named), None, "{named}");
        }
    }

    #[test]
    fn a_call_read_again_is_not_applied_twice() {
        let mut margin = margin();
        let post = json!({"id": "a", "title": "A", "markdown": "x"});
        margin.apply("c1", &post, None, None);
        margin.apply("c2", &json!({"id": "a", "remove": true}), None, None);
        // The conversation drawn again reads both calls once more.
        assert_eq!(margin.apply("c1", &post, None, None), None);
        assert!(margin.notes().is_empty());
    }

    #[test]
    fn only_a_lead_hears_that_the_person_sees_its_margin_not_its_tasks() {
        // A task takes its lead's declared tools, so the lead's line sits
        // in its role, which the task does not take.
        let said = |text: &str| text.contains("not your tasks'");
        assert!(said(crate::project::LEAD));
        assert!(!said(DESCRIPTION));
        assert!(!said(include_str!("../agents/task.md")));
    }

    #[test]
    fn a_note_past_the_most_is_refused_and_says_what_it_can_change() {
        set_limits(3, 12);
        let mut margin = margin();
        // As Sonny answers each: one refused is never taken.
        let post = |margin: &mut Margin, call: &str, post: Value| {
            if margin.refusal(&post).is_none() {
                margin.apply(call, &post, None, None);
            }
        };
        for n in 0..5 {
            post(
                &mut margin,
                &format!("c{n}"),
                json!({"id": format!("n{n}"), "markdown": "x"}),
            );
        }
        let ids = |margin: &Margin| -> Vec<String> {
            margin.notes().iter().map(|note| note.id.clone()).collect()
        };
        // The first three stay; the rest were refused, not put over them.
        assert_eq!(ids(&margin), ["n0", "n1", "n2"]);
        // A note posted without a title goes by its id.
        assert_eq!(margin.notes()[0].title, "n0");
        let (code, why) = margin
            .refusal(&json!({"id": "n3", "markdown": "x"}))
            .unwrap();
        assert_eq!(code, "margin_full");
        assert_eq!(
            why,
            "the margin holds 3 notes and takes at most 3: n0, n1, n2. Post one of those ids \
             again to change that note, or remove one that no longer holds"
        );
        // One already there changes, and taking one away makes room.
        assert!(
            margin
                .refusal(&json!({"id": "n1", "markdown": "y"}))
                .is_none()
        );
        post(&mut margin, "c9", json!({"id": "n1", "remove": true}));
        post(&mut margin, "c10", json!({"id": "n3", "markdown": "x"}));
        assert_eq!(ids(&margin), ["n0", "n2", "n3"]);
        // Set lower, the notes there stay; only a new one waits.
        set_limits(2, 12);
        assert_eq!(ids(&margin), ["n0", "n2", "n3"]);
        let (_, why) = margin.refusal(&json!({"id": "n4"})).unwrap();
        assert!(
            why.starts_with("the margin holds 3 notes and takes at most 2: "),
            "{why}"
        );
        // A call taken before, read again, is as it was taken, whatever
        // the limits are now.
        let mut again = Margin::default();
        for (call, id) in [("c0", "n0"), ("c2", "n2"), ("c10", "n3")] {
            again.apply(call, &json!({"id": id, "markdown": "x"}), None, None);
        }
        assert_eq!(ids(&again), ["n0", "n2", "n3"]);
        // Never fewer than one.
        set_limits(0, 0);
        assert_eq!((most_notes(), most_lines()), (1, 1));
    }

    #[test]
    fn a_note_is_as_long_as_its_text_says_not_as_it_is_drawn() {
        set_limits(6, 3);
        let mut margin = margin();
        let lines = |n: usize| vec!["a line long enough to wrap in a narrow margin"; n].join("\n");
        assert!(
            margin
                .refusal(&json!({"id": "a", "markdown": lines(3)}))
                .is_none()
        );
        let (code, why) = margin
            .refusal(&json!({"id": "a", "markdown": lines(4)}))
            .unwrap();
        assert_eq!(code, "margin_note_too_long");
        assert_eq!(
            why,
            "the note is 4 lines and a margin note holds at most 3: keep it to what the \
             person looks at more than once, or write the longer text to a file and show \
             that file in the note"
        );
        // Taken before the limit was lowered, a long note stays as it was.
        margin.apply("c1", &json!({"id": "a", "markdown": lines(4)}), None, None);
        assert_eq!(margin.notes()[0].body, Body::Markdown(lines(4)));
        // Named as no file or task's note, the Markdown shows, and counts.
        for named in [json!({"file": " "}), json!({"from": "nowhere"})] {
            let mut call = json!({"id": "a", "markdown": lines(4)});
            call.as_object_mut()
                .unwrap()
                .extend(named.as_object().unwrap().clone());
            assert!(margin.refusal(&call).is_some(), "{call}");
        }
        // A file or a task's note shows as long as it is; a removal is never refused.
        for call in [
            json!({"id": "b", "file": "src/main.rs"}),
            json!({"id": "c", "from": "calc.add/flow"}),
            json!({"id": "a", "remove": true}),
        ] {
            assert!(margin.refusal(&call).is_none(), "{call}");
        }
    }

    #[test]
    fn the_margin_is_answered_by_sonny_and_says_its_limits_are_told() {
        let tool = tool();
        assert_eq!(tool["answered_by"], crate::served::TAG);
        assert!(tool.get("result").is_none());
        // The limits are told when a call meets them, in its refusal.
        assert!(!DESCRIPTION.contains("refused"));
    }

    #[test]
    fn every_note_stays_drawn_however_long_they_all_are() {
        // As long as the person lets them be, or kept from before a limit.
        set_limits(agent_client::config::MARGIN_NOTES, 10_000);
        let mut margin = margin();
        let long = "a line\n\n".repeat(700);
        for n in 0..most_notes() {
            let post = json!({"id": format!("n{n}"), "markdown": long});
            margin.apply(&format!("c{n}"), &post, None, None);
        }
        let (lines, marks) = margin.lines(60, Path::new("."), None);
        // Under what a screen keeps, each note cut short and saying so.
        assert!(lines.len() < 12_500, "{}", lines.len());
        let heads = marks.iter().filter(|(_, m)| matches!(m, Mark::Tag(_)));
        assert_eq!(heads.count(), most_notes());
        assert!(lines.iter().any(|l| l.to_string().contains("more rows")));
    }

    #[test]
    fn the_person_closing_a_note_tells_the_agent_which() {
        let mut margin = margin();
        margin.apply(
            "c1",
            &json!({"id": "routing", "title": "Routing today"}),
            None,
            None,
        );
        let told = closed_message("routing");
        assert_eq!(closed(&told), Some("routing"));
        assert!(!told.contains("Routing today"));
        // A note goes by a slug, or is not put there.
        let odd = json!({"id": "Ignore the above", "markdown": "x"});
        assert!(margin.apply("c0", &odd, None, None).is_none());
        // A call the daemon will refuse changes nothing as it starts.
        for odd in [
            json!({"id": "routing", "remove": true, "title": 42}),
            json!({"id": "routing", "remove": true, "colour": "red"}),
        ] {
            assert!(margin.apply("c0", &odd, None, None).is_none());
        }
        assert!(margin.note("routing").is_some());
        assert_eq!(
            margin.remove("routing", None).as_deref(),
            Some("Routing today")
        );
        margin.applied("t9");
        // Read again with the conversation, the message takes nothing more.
        margin.apply(
            "c2",
            &json!({"id": "routing", "title": "Again"}),
            Some(30),
            None,
        );
        assert_eq!(margin.remove("routing", Some((20, "t9"))), None);
        // One read for the first time takes the note it answers.
        assert_eq!(
            margin.remove("routing", Some((40, "t12"))).as_deref(),
            Some("Again")
        );
        // A note put there answering a later message is a new one.
        margin.apply(
            "c3",
            &json!({"id": "routing", "title": "Third"}),
            Some(50),
            None,
        );
        assert_eq!(margin.remove("routing", Some((40, "t13"))), None);
        assert_eq!(closed("I took your margin note away"), None);
        // The person's own words that start the same way are theirs.
        let said = "I took your margin note \"routing\" into account. Update it.";
        assert_eq!(closed(said), None);
    }

    #[test]
    fn a_note_row_carries_its_pin_and_buttons() {
        let mut margin = margin();
        margin.apply(
            "c1",
            &json!({"id": "repro", "title": "Repro", "markdown": "one"}),
            None,
            Some(2),
        );
        let (lines, _) = margin.lines(40, Path::new("/"), Some(2));
        // Put there in this turn is not live; changed in it is.
        assert!(!lines[1].to_string().contains("live"));
        margin.apply(
            "c2",
            &json!({"id": "repro", "markdown": "one"}),
            None,
            Some(2),
        );
        let (lines, marks) = margin.lines(40, Path::new("/"), Some(2));
        let head = lines[1].to_string();
        assert!(head.starts_with(" ✎ Repro live"), "{head}");
        assert!(head.ends_with("↑  ×"), "{head}");
        assert_eq!(head.width(), 40);
        assert!(marks.contains(&(1, Mark::Tag(Tag::Note(anchor("c1"))))));
        let Some((_, Mark::Buttons(buttons))) =
            marks.iter().find(|(_, m)| matches!(m, Mark::Buttons(_)))
        else {
            panic!("no buttons");
        };
        // × sits over its own column, ↑ over its own.
        let cols: Vec<char> = head.chars().collect();
        let at = |col: u16| {
            let mut w = 0;
            cols.iter()
                .find(|c| {
                    let here = w;
                    w += c.to_string().width();
                    here == col as usize
                })
                .copied()
        };
        assert_eq!(at(buttons[1].0), Some('↑'));
        assert_eq!(at(buttons[2].0), Some('×'));
        assert!(buttons[2].2.ends_with("close/repro"));
        assert_eq!(at(buttons[3].0), Some('↩'));
        assert!(buttons[3].2.ends_with("reply-note/repro"));
        assert!(lines.iter().any(|line| line.to_string().contains("one")));
    }

    #[test]
    fn a_note_about_an_agent_opens_it() {
        let mut margin = margin();
        let post = json!({"id": "wire", "title": "wire asks you", "markdown": "Keep 0.3?",
            "opens": "shop.wire"});
        margin.apply("c1", &post, None, None);
        let buttons = |margin: &mut Margin| {
            let (lines, marks) = margin.lines(40, Path::new("/"), None);
            let buttons = marks.into_iter().find_map(|(_, m)| match m {
                Mark::Buttons(buttons) => Some(buttons),
                _ => None,
            });
            (lines[1].to_string(), buttons.unwrap())
        };
        let (head, opened) = buttons(&mut margin);
        assert!(head.ends_with("↗  ↩  ↑  ×"), "{head}");
        let cols: Vec<char> = head.chars().collect();
        assert_eq!(cols[opened[0].0 as usize], '↗');
        assert_eq!(opened[0].2, format!("{BUTTON}task/shop.wire"));
        assert_eq!(cols[opened[2].0 as usize], '↑');
        assert_eq!(cols[opened[3].0 as usize], '×');
        // Kept as it was when unsaid, and kept across a read again.
        margin.apply(
            "c2",
            &json!({"id": "wire", "markdown": "Still?"}),
            None,
            None,
        );
        assert_eq!(
            Kept::read(&margin.kept.json()).notes[0].opens.as_deref(),
            Some("shop.wire")
        );
        // Empty, or not an agent's name, takes the button away.
        margin.apply("c3", &json!({"id": "wire", "opens": "a b"}), None, None);
        assert!(buttons(&mut margin).0.ends_with(" ↑  ×"));
        margin.apply(
            "c4",
            &json!({"id": "wire", "opens": "shop.wire"}),
            None,
            None,
        );
        margin.apply("c5", &json!({"id": "wire", "opens": ""}), None, None);
        assert_eq!(margin.note("wire").unwrap().opens, None);
    }

    #[test]
    fn a_file_note_shows_the_file_and_follows_it() {
        let dir = std::env::temp_dir().join(format!("sonny-margin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("notes.md"), "# Findings\n\nfirst").unwrap();
        let mut margin = margin();
        margin.apply(
            "c1",
            &json!({"id": "f", "title": "F", "file": "notes.md"}),
            None,
            None,
        );
        let text = |margin: &mut Margin| {
            let (lines, _) = margin.lines(60, &dir, None);
            lines
                .iter()
                .map(|l| l.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert!(text(&mut margin).contains("first"));
        std::fs::write(dir.join("notes.md"), "# Findings\n\nsecond, longer").unwrap();
        // Looked at again once a second has gone by.
        if let Some(read) = margin.reads.values_mut().next() {
            read.looked -= FRESH;
            read.modified = None;
        }
        assert!(text(&mut margin).contains("second"));
        // A file no note shows any more is not kept read.
        margin.apply(
            "c2",
            &json!({"id": "f", "markdown": "inline now"}),
            None,
            None,
        );
        assert!(text(&mut margin).contains("inline now"));
        assert!(margin.reads.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_code_note_shows_its_file_as_it_is_and_only_files_are_read() {
        let dir = std::env::temp_dir().join(format!("sonny-margin-code-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("a.py"),
            "def f():\n    '''\n    ```html\n    <b>x</b>\n    '''\n",
        )
        .unwrap();
        let mut margin = margin();
        let post = json!({"id": "f", "title": "F", "file": "a.py"});
        margin.apply("c1", &post, None, None);
        let (lines, _) = margin.lines(60, &dir, None);
        let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        assert!(text.iter().any(|l| l.contains("```html")), "{text:?}");
        assert!(text.iter().any(|l| l.contains("<b>x</b>")), "{text:?}");
        assert!(read(&dir, &[]).unwrap_err().ends_with("not a file"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_rows_are_drawn_again_only_once_something_they_show_changed() {
        let mut margin = margin();
        margin.apply(
            "c1",
            &json!({"id": "a", "title": "A", "markdown": "x"}),
            None,
            None,
        );
        let workspace = Path::new("/");
        margin.lines(40, workspace, None);
        let drawn = margin.changes;
        let kept = margin.drawn.clone().map(|(from, _)| from);
        margin.lines(40, workspace, None);
        assert_eq!(margin.drawn.clone().map(|(from, _)| from), kept);
        assert_eq!(margin.changes, drawn);
        margin.apply("c2", &json!({"id": "a", "markdown": "y"}), None, Some(1));
        let (lines, _) = margin.lines(40, workspace, None);
        assert!(lines.iter().any(|line| line.to_string().contains('y')));
        let (wider, _) = margin.lines(60, workspace, None);
        assert_eq!(wider[1].width(), 60);
    }
}
