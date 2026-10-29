//! The margin beside the conversation: notes a bot keeps there with the
//! `margin` tool Sonny declares on it, each pinned to the point in the
//! conversation where it was first put, under a glance line of what runs
//! for the agent. A marker where a note was put scrolls the margin to it,
//! and the note's ↑ scrolls the conversation back there.
//!
//! The calls are the record, in the bot's history; the margin as it
//! stands is kept beside the person's reactions, by the bot's key, so a
//! window opened later shows it without reading the whole history.
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

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
/// The most notes a margin holds: a new one past it lets the oldest go.
const MOST_NOTES: usize = 20;
/// The most of a note's Markdown kept, and of a file read for one.
const NOTE_BYTES: usize = 64 * 1024;
/// The most lines of a file a note shows.
const FILE_LINES: usize = 400;
/// The most rows a note is drawn in, so all the notes together stay
/// under what a [`crate::screen::Screen`] keeps.
const NOTE_ROWS: usize = 600;
/// How many of the calls and messages applied are remembered, so one
/// read again as the conversation is drawn again is not applied twice.
const SEEN: usize = 1024;
/// How often a file a note shows is looked at for changes.
const FRESH: Duration = Duration::from_secs(1);

/// The `margin` tool as Sonny declares it on a bot.
pub fn tool() -> Value {
    json!({
        "name": TOOL,
        "description": "Keep something beside the conversation, in the margin the person sees next to it: \
            a diagram, a table of findings, the file you are discussing, or a live view you update while \
            you work. A note stays pinned to this point in the conversation, and the person can jump \
            between it and here. Posting an id again replaces that note in place, so update one note \
            rather than posting many; remove takes it away. Give markdown (rendered as your replies are, \
            Mermaid included) or file (a path, shown as the file stands and kept fresh as it changes). \
            Never put a question or anything the person must answer there, and never say the same thing \
            in your reply: point to the note instead.",
        "parameters": {
            "type": "object",
            "properties": {
                "id": {"type": "string", "pattern": "^[a-z0-9][a-z0-9-]{0,39}$",
                    "description": "The note's short name, such as routing or repro."},
                "title": {"type": "string", "minLength": 1, "maxLength": 80,
                    "description": "A few words, shown on the note."},
                "markdown": {"type": "string", "maxLength": NOTE_BYTES,
                    "description": "The note, as Markdown."},
                "file": {"type": "string", "minLength": 1,
                    "description": "Or a file to show, absolute or from your folder."},
                "remove": {"type": "boolean", "description": "true takes the note away."}
            },
            "required": ["id"],
            "additionalProperties": false
        },
        "result": "In the margin."
    })
}

/// What a note shows.
#[derive(Clone, Debug, PartialEq)]
pub enum Body {
    Markdown(String),
    File(String),
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

#[derive(Default)]
struct Kept {
    notes: Vec<Note>,
    /// The calls and messages applied, newest last.
    seen: VecDeque<String>,
    /// The person closed the margin: it stays closed until they open it.
    closed: bool,
}

impl Kept {
    fn json(&self) -> Value {
        let notes: Vec<Value> = (self.notes.iter())
            .map(|note| {
                let (kind, body) = match &note.body {
                    Body::Markdown(text) => ("markdown", text),
                    Body::File(path) => ("file", path),
                };
                json!({"id": note.id, "title": note.title, kind: body, "call": note.call,
                    "after": note.after, "turn": note.turn})
            })
            .collect();
        json!({"notes": notes, "seen": self.seen, "closed": self.closed})
    }

    /// What [`Kept::json`] wrote; a note it cannot read is left out.
    fn read(json: &Value) -> Self {
        let text = |note: &Value, key: &str| note[key].as_str().map(str::to_owned);
        let notes = (json["notes"].as_array().into_iter().flatten())
            .filter_map(|note| {
                let body = match text(note, "file") {
                    Some(path) => Body::File(path),
                    None => Body::Markdown(text(note, "markdown")?),
                };
                Some(Note {
                    id: text(note, "id")?,
                    title: text(note, "title")?,
                    body,
                    call: text(note, "call")?,
                    after: note["after"].as_i64(),
                    turn: note["turn"].as_i64(),
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
        }
    }
}

/// A file a note shows, as last read: its time, when it was looked at,
/// and its text.
struct Read {
    modified: Option<SystemTime>,
    looked: Instant,
    text: Result<String, String>,
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

    pub fn notes(&self) -> &[Note] {
        &self.kept.notes
    }

    pub fn note(&self, id: &str) -> Option<&Note> {
        self.kept.notes.iter().find(|note| note.id == id)
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
        self.save();
        change
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
        let body = match (arguments["file"].as_str(), arguments["markdown"].as_str()) {
            (Some(file), _) if !file.trim().is_empty() => Some(Body::File(file.trim().to_owned())),
            (_, Some(markdown)) => Some(Body::Markdown(cut(markdown, NOTE_BYTES).to_owned())),
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
                });
                if self.kept.notes.len() > MOST_NOTES {
                    self.kept.notes.remove(0);
                }
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
        let files: Vec<PathBuf> = (self.kept.notes.iter())
            .filter_map(|note| match &note.body {
                Body::File(path) => Some(workspace.join(path)),
                Body::Markdown(_) => None,
            })
            .collect();
        // Only the files notes show now are kept read.
        self.reads.retain(|path, _| files.contains(path));
        for path in &files {
            self.look(path);
        }
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

    fn draw(&mut self, width: usize, workspace: &Path, live: Option<i64>) -> Rows {
        let mut lines = Vec::new();
        let mut marks = Vec::new();
        if self.kept.notes.is_empty() {
            lines.push(Line::default());
            lines.push(
                Line::from(" Notes the agent keeps beside the conversation show here.").dim(),
            );
            return (lines, marks);
        }
        let inner = width.saturating_sub(3).max(10);
        let notes = self.kept.notes.clone();
        for note in &notes {
            lines.push(Line::default());
            let at = lines.len();
            let (head, buttons) = head(note, width, live.is_some() && note.turn == live);
            lines.push(head);
            marks.push((at, Mark::Tag(Tag::Note(anchor(&note.call)))));
            marks.push((at, Mark::Buttons(buttons)));
            let mut body = match &note.body {
                Body::Markdown(text) => markdown(text, inner, Markdown::in_folder(workspace)),
                Body::File(path) => self.file_lines(workspace, path, inner),
            };
            if body.len() > NOTE_ROWS {
                let more = body.len() - NOTE_ROWS;
                body.truncate(NOTE_ROWS);
                body.push(Line::from(format!("  … {more} more rows")).dim());
            }
            lines.extend(
                body.into_iter()
                    .map(|line| crate::transcript::indent(line, 3)),
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
                let text = read(path);
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
            Err(error) => return vec![Line::from(error.clone()).dim()],
        };
        let extension = (path.extension().and_then(|e| e.to_str()))
            .unwrap_or_default()
            .to_lowercase();
        let lines: Vec<&str> = text.lines().collect();
        let shown = lines.len().min(FILE_LINES);
        // Its pictures read from beside it.
        let folder = path.parent().unwrap_or(workspace);
        let mut out = match extension.as_str() {
            "md" | "markdown" | "mdx" => markdown(
                &lines[..shown].join("\n"),
                width,
                Markdown::document(folder),
            ),
            // Code as code, whatever it holds.
            _ => {
                let mut code = crate::highlight::Code::file(&path, lines.first().unwrap_or(&""));
                let lead = || Span::raw("  ");
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
            out.push(Line::from(format!("… {} more lines", lines.len() - shown)).dim());
        }
        out
    }
}

/// `text` read by `markdown`, as a preview draws a `.md` file: as
/// replies read, without a reply's bullet.
fn markdown(text: &str, width: usize, mut markdown: Markdown) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for line in text.lines() {
        out.extend(markdown.feed(line, width, false));
    }
    out.extend(markdown.end(width, false));
    out
}

/// The file at `path`, as much of it as a note shows.
fn read(path: &Path) -> Result<String, String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    // A pipe or a device could hold the window up: only files are read.
    match std::fs::metadata(path) {
        Ok(meta) if !meta.is_file() => return Err(format!("{}: not a file", path.display())),
        _ => {}
    }
    std::fs::File::open(path)
        .and_then(|f| f.take(NOTE_BYTES as u64 * 4).read_to_end(&mut bytes))
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
    let right = "↑  × ";
    let tail = match live {
        true => " live",
        false => "",
    };
    let room = width.saturating_sub(right.width() + tail.width() + 4);
    let title = crate::transcript::cut(&note.title, room);
    let used = 3 + title.width() + tail.width();
    let gap = width.saturating_sub(used + right.width()).max(1);
    let up = (used + gap) as u16;
    let line = Line::from(vec![
        Span::raw(" ✎ ").fg(Color::Yellow),
        Span::raw(title).bold(),
        Span::raw(tail).fg(Color::Green),
        Span::raw(" ".repeat(gap)),
        Span::raw("↑").dim(),
        Span::raw("  "),
        Span::raw("×").dim(),
    ]);
    let id = &note.id;
    let buttons = vec![
        (0, up.saturating_sub(1), format!("{BUTTON}jump/{id}")),
        (up, up + 1, format!("{BUTTON}jump/{id}")),
        (up + 3, up + 4, format!("{BUTTON}close/{id}")),
    ];
    (line, buttons)
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

/// What runs for the agent, as the margin's first row says it.
#[derive(Default)]
pub struct Glance {
    pub name: String,
    /// Its tasks at work.
    pub running: usize,
    /// Questions and calls waiting on the person.
    pub waiting: usize,
    /// Triggers and subscriptions.
    pub behind: usize,
    pub context: Option<u64>,
}

impl Glance {
    /// The row, and where its buttons are: each fact opens its view.
    pub fn line(&self) -> (Line<'static>, crate::screen::Buttons) {
        let mut spans = vec![Span::raw(" "), Span::raw(self.name.clone()).bold()];
        let mut buttons = Vec::new();
        let mut at = 1 + self.name.width();
        let mut fact = |spans: &mut Vec<Span<'static>>,
                        text: String,
                        span: Span<'static>,
                        url: Option<&str>| {
            spans.push(Span::raw(" · ").dim());
            at += 3;
            let width = text.width();
            if let Some(url) = url {
                buttons.push((at as u16, (at + width) as u16, format!("{BUTTON}{url}")));
            }
            spans.push(span);
            at += width;
        };
        if self.running > 0 {
            let text = format!("{} running", self.running);
            fact(
                &mut spans,
                text.clone(),
                Span::raw(text).fg(Color::Cyan),
                Some("status"),
            );
        }
        if self.waiting > 0 {
            let text = format!("{} need you", self.waiting);
            fact(
                &mut spans,
                text.clone(),
                Span::raw(text).fg(Color::Yellow),
                Some("status"),
            );
        }
        if self.behind > 0 {
            let text = format!("↻ {}", self.behind);
            fact(
                &mut spans,
                text.clone(),
                Span::raw(text),
                Some("background"),
            );
        }
        if let Some(context) = self.context {
            let text = crate::transcript::tokens(context);
            fact(&mut spans, text.clone(), Span::raw(text).dim(), None);
        }
        (Line::from(spans), buttons)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn margin() -> Margin {
        Margin::default()
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
    fn the_oldest_note_goes_past_the_most() {
        let mut margin = margin();
        for n in 0..=MOST_NOTES {
            let post = json!({"id": format!("n{n}"), "markdown": "x"});
            margin.apply(&format!("c{n}"), &post, None, None);
        }
        assert_eq!(margin.notes().len(), MOST_NOTES);
        assert_eq!(margin.notes()[0].id, "n1");
        // A note posted without a title goes by its id.
        assert_eq!(margin.notes()[0].title, "n1");
    }

    #[test]
    fn every_note_stays_drawn_however_long_they_all_are() {
        let mut margin = margin();
        let long = "a line\n\n".repeat(700);
        for n in 0..MOST_NOTES {
            let post = json!({"id": format!("n{n}"), "markdown": long});
            margin.apply(&format!("c{n}"), &post, None, None);
        }
        let (lines, marks) = margin.lines(60, Path::new("."), None);
        // Under what a screen keeps, each note cut short and saying so.
        assert!(lines.len() < 12_500, "{}", lines.len());
        let heads = marks.iter().filter(|(_, m)| matches!(m, Mark::Tag(_)));
        assert_eq!(heads.count(), MOST_NOTES);
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
        assert_eq!(head.width(), 40 - 1);
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
        assert!(lines.iter().any(|line| line.to_string().contains("one")));
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
        assert!(read(&dir).unwrap_err().ends_with("not a file"));
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
        assert_eq!(wider[1].width(), 60 - 1);
    }

    #[test]
    fn the_glance_line_says_only_what_runs_and_each_fact_opens_its_view() {
        let quiet = Glance {
            name: "calc".into(),
            ..Glance::default()
        };
        assert_eq!(quiet.line().0.to_string(), " calc");
        let busy = Glance {
            name: "calc".into(),
            running: 2,
            waiting: 1,
            behind: 3,
            context: Some(41_000),
        };
        let (line, buttons) = busy.line();
        let text = line.to_string();
        assert!(
            text.starts_with(" calc · 2 running · 1 need you · ↻ 3 · "),
            "{text}"
        );
        let urls: Vec<&str> = buttons.iter().map(|b| b.2.as_str()).collect();
        assert_eq!(urls, ["sonny:status", "sonny:status", "sonny:background"]);
        let slice = |(from, to): (u16, u16)| -> String {
            text.chars()
                .skip(from as usize)
                .take((to - from) as usize)
                .collect()
        };
        assert_eq!(slice((buttons[0].0, buttons[0].1)), "2 running");
        assert_eq!(slice((buttons[2].0, buttons[2].1)), "↻ 3");
    }
}
