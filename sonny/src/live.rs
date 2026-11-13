//! Live templates a window shows, kept filled as what they read changes:
//! margin notes and the blocks an agent puts in the conversation with the
//! `live` tool. One [`Watch`] per window looks at each no more than once a
//! [`FRESH`], stats each file it read once however many read it, and fills
//! one again only once what it read last changed ([`crate::template`]).
//! A fill never reaches the agent's history: the call that wrote the
//! template is the record, as Codex's plan is its call.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use ratatui::{
    style::{Color, Stylize},
    text::{Line, Span},
};
use serde_json::{Value, json};

use crate::margin::Blanks;
use crate::template::{Reads, Stamp};
use crate::transcript::Markdown;

/// How often a live template is looked at for what it read changing.
pub const FRESH: Duration = Duration::from_secs(1);

/// The name of the tool an agent shows something live in the
/// conversation with.
pub const TOOL: &str = "live";

/// The most lines a block in the conversation shows; the rest are a
/// count, as a margin note's past its lines are.
pub const LINES: usize = 40;

/// The `live` tool as Sonny declares it on a bot.
pub fn tool() -> Value {
    json!({
        "name": TOOL,
        "description": "Show the person something that changes, live in the conversation: Markdown with Jinja blanks \
            Sonny fills now and keeps filled, such as the time or what a source says now. The answer is what it \
            shows now. Write your reply around it, not repeating what will change. For what does not change, just reply.",
        "parameters": {
            "type": "object",
            "properties": {
                "markdown": {"type": "string", "minLength": 1, "maxLength": crate::template::OUTPUT,
                    "description": "Blanks: source('name').latest/.topics, file('path'), tasks(), task('name'), now(), |ago, |time, |fromjson."}
            },
            "required": ["markdown"],
            "additionalProperties": false
        },
        "answered_by": crate::served::TAG
    })
}

/// What a `live` call is answered, filled from `data` as the tool host
/// has it: refused as every live template is when it could never fill;
/// else what it shows now and when, as the agent reads it, at most what a
/// prompt keeps, or why it does not fill yet, as a blank that names a
/// task the window fills.
pub fn answer(
    template: &str,
    data: &crate::template::Data,
) -> Result<String, (&'static str, String)> {
    crate::template::refusal(template, data, crate::template::To::Window)
        .map_err(|why| (crate::template::REFUSED, why))?;
    let at = crate::asks::clock(data.now, data.now);
    let (filled, reads) = crate::template::render(template, data);
    Ok(match filled {
        // The host does not know where the tasks stand: what it filled
        // from none would tell the agent a count the window never shows.
        _ if reads.tasks => "Shown live in the conversation, kept filled as it changes; \
            it reads the tasks, which only the window knows, so it does not fill here."
            .to_owned(),
        Ok(text) => agent_client::memory::cut_filled(format!(
            "Shown live in the conversation, kept filled as it changes. At {at} it shows:\n{text}"
        )),
        Err(why) => format!("Shown live in the conversation; it does not fill here yet ({why})."),
    })
}

/// A live template as last filled: from what, to what, and what it read
/// as that stood then.
pub struct Filled {
    pub template: String,
    pub folder: PathBuf,
    /// What it last filled to; kept while a later fill fails.
    pub text: Option<String>,
    /// Why the last fill failed, when it did.
    pub error: Option<String>,
    pub reads: Reads,
    /// The sources set up when it filled: one added or taken away since
    /// may change what it shows.
    sources: Arc<Vec<String>>,
    tasks_read: u64,
    minute: i64,
    pub looked: Instant,
}

/// A window's live templates as last filled, by a key of its own.
#[derive(Default)]
pub struct Watch {
    filled: HashMap<String, Filled>,
    /// Each file a look this moment found, so a file many read is looked
    /// at once: kept no longer than a look takes.
    stamps: HashMap<PathBuf, Stamp>,
    stamped: Option<Instant>,
}

impl Watch {
    /// `key` as last filled.
    pub fn get(&self, key: &str) -> Option<&Filled> {
        self.filled.get(key)
    }

    /// Whether `key` is due a look: never filled, filled from another
    /// template or folder, or not looked at for a [`FRESH`].
    pub fn due(&self, key: &str, template: &str, folder: &Path) -> bool {
        (self.filled.get(key)).is_none_or(|filled| {
            filled.looked.elapsed() >= FRESH
                || filled.template != template
                || filled.folder != folder
        })
    }

    /// Only the keys `keep` says stay; whether any went.
    pub fn retain(&mut self, keep: impl Fn(&str) -> bool) -> bool {
        let was = self.filled.len();
        self.filled.retain(|key, _| keep(key));
        self.filled.len() != was
    }

    /// `key` filled again from `template` in `folder` once what it read
    /// last changed: a source's log or a file, the tasks, the sources set
    /// up, or the minute, when it read the clock. A fill that fails keeps
    /// what it last showed, or `last` when it never filled here. Whether
    /// what it shows changed.
    pub fn fill(
        &mut self,
        key: &str,
        (template, folder): (&str, &Path),
        blanks: &Blanks,
        last: Option<String>,
    ) -> bool {
        let now = Instant::now();
        let epoch = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        // One look's stamps serve every template it fills.
        if self.stamped.is_none_or(|at| at.elapsed() >= FRESH / 4) {
            self.stamps.clear();
            self.stamped = Some(now);
        }
        if let Some(filled) = self.filled.get_mut(key)
            && filled.template == template
            && filled.folder == folder
        {
            filled.looked = now;
            let stamps = &mut self.stamps;
            let again = (filled.reads.tasks && filled.tasks_read != blanks.tasks_read)
                || !Arc::ptr_eq(&filled.sources, &blanks.sources)
                || (filled.reads.clock && filled.minute != epoch / 60)
                || (filled.reads.seen.iter()).any(|(path, was)| {
                    *stamps
                        .entry(path.clone())
                        .or_insert_with(|| crate::template::stamp(path))
                        != *was
                });
            if !again {
                return false;
            }
        }
        let data = crate::template::Data {
            store: blanks.store.clone(),
            sources: blanks.sources.clone(),
            folder: folder.to_owned(),
            tasks: blanks.tasks.clone(),
            now: epoch,
        };
        let (rendered, reads) = crate::template::render(template, &data);
        let was = self.filled.remove(key);
        let (text, error) = match rendered {
            Ok(text) => (Some(text), None),
            Err(error) => {
                let shown = was.as_ref().and_then(|was| was.text.clone());
                (shown.or(last), Some(error))
            }
        };
        let changed = was
            .as_ref()
            .is_none_or(|was| was.text != text || was.error != error || was.reads != reads);
        let filled = Filled {
            template: template.to_owned(),
            folder: folder.to_owned(),
            text,
            error,
            reads,
            sources: blanks.sources.clone(),
            tasks_read: blanks.tasks_read,
            minute: epoch / 60,
            looked: now,
        };
        self.filled.insert(key.to_owned(), filled);
        changed
    }

    /// Every template looked at again as if a [`FRESH`] had passed.
    #[cfg(test)]
    pub fn later(&mut self) {
        for filled in self.filled.values_mut() {
            filled.looked -= FRESH;
        }
        self.stamped = None;
    }
}

/// `filled`'s rows `width` wide after `lead`, its pictures read from
/// `folder`: what it follows, its text cut at `most` lines, then why its
/// last fill failed, when it did; `…` before its first fill.
pub fn lines(
    filled: Option<&Filled>,
    width: usize,
    folder: &Path,
    most: usize,
    lead: &'static str,
) -> Vec<Line<'static>> {
    let Some(filled) = filled else {
        return vec![Line::from(format!("{lead}…")).dim()];
    };
    let mut out = follows(&filled.reads, &filled.folder, width, lead);
    if let Some(text) = &filled.text {
        let lines: Vec<&str> = text.lines().collect();
        let shown = lines.len().min(most);
        let mut markdown = Markdown::in_folder(folder);
        let text = lines[..shown].join("\n");
        let leads = (Line::from(lead), Line::from(lead));
        out.extend(crate::transcript::markdown(
            &mut markdown,
            &text,
            width,
            leads,
        ));
        if lines.len() > shown {
            let more = lines.len() - shown;
            out.push(Line::from(format!("{lead}… {more} more lines")).dim());
        }
    }
    if let Some(error) = &filled.error {
        let said = Line::from(format!("⟨{error}⟩")).dim();
        out.extend(crate::transcript::wrap(
            said,
            width,
            Span::raw(lead),
            Span::raw(lead),
        ));
    }
    out
}

/// What a live template follows, after `lead`, so a filled one never
/// passes for one written: the sources it read, by icon, and since when
/// one is down, the tasks, and its files from `folder`. None for one that
/// reads only the clock.
fn follows(reads: &Reads, folder: &Path, width: usize, lead: &'static str) -> Vec<Line<'static>> {
    let icons = match reads.sources.is_empty() {
        true => Default::default(),
        false => crate::background::source_icons(),
    };
    let mut spans = vec![Span::raw("follows ").dim()];
    let mut first = true;
    let mut gap = |spans: &mut Vec<Span<'static>>| {
        if !std::mem::take(&mut first) {
            spans.push(Span::raw(", ").dim());
        }
    };
    for name in &reads.sources {
        gap(&mut spans);
        let named = match icons.get(name) {
            Some(icon) => format!("{icon} {name}"),
            None => name.clone(),
        };
        spans.push(Span::raw(named).dim());
        if let Some(since) = reads.down.get(name) {
            let down = format!(" down since {}", crate::stamp::when(*since));
            spans.push(Span::raw(down).fg(Color::Red));
        }
    }
    if reads.tasks {
        gap(&mut spans);
        spans.push(Span::raw("tasks").dim());
    }
    for path in &reads.files {
        gap(&mut spans);
        let name = path.strip_prefix(folder).unwrap_or(path);
        spans.push(Span::raw(name.display().to_string()).dim());
    }
    if first {
        return Vec::new();
    }
    crate::transcript::wrap(Line::from(spans), width, Span::raw(lead), Span::raw(lead))
}

/// A block in the conversation: its first row, `◇ live`, then its rows
/// as last filled, `width` wide, files read from `folder`.
pub fn block(filled: Option<&Filled>, width: usize, folder: &Path) -> Vec<Line<'static>> {
    let mut rows = vec![Line::from(vec![
        Span::raw("◇ ").dim(),
        Span::raw("live").fg(Color::Cyan),
    ])];
    rows.extend(lines(filled, width, folder, LINES, "  "));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blanks() -> Blanks {
        Blanks {
            tasks: Arc::new(vec![json!({"name": "calc.add", "state": "working"})]),
            ..Default::default()
        }
    }

    #[test]
    fn a_block_fills_again_only_once_what_it_read_changes() {
        let dir = std::env::temp_dir().join(format!("sonny-live-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("state.json"), r#"{"state": "open"}"#).unwrap();
        let template = "PR is {{ (file('state.json') | fromjson).state }}";
        let mut watch = Watch::default();
        let mut blanks = blanks();
        assert!(watch.due("a", template, &dir));
        assert!(
            watch.fill("a", (template, &dir), &blanks, None),
            "a first fill"
        );
        assert!(!watch.due("a", template, &dir), "looked at once a second");
        let shown = |watch: &Watch| watch.get("a").and_then(|f| f.text.clone());
        assert_eq!(shown(&watch).as_deref(), Some("PR is open"));
        watch.later();
        assert!(
            !watch.fill("a", (template, &dir), &blanks, None),
            "nothing changed"
        );
        // A new read of the tasks changes nothing it did not read.
        blanks.tasks_read += 1;
        watch.later();
        assert!(!watch.fill("a", (template, &dir), &blanks, None));
        std::fs::write(dir.join("state.json"), r#"{"state": "merged", "x": 1}"#).unwrap();
        watch.later();
        assert!(watch.fill("a", (template, &dir), &blanks, None));
        assert_eq!(shown(&watch).as_deref(), Some("PR is merged"));
        // One that fails later keeps what it showed, and says why.
        std::fs::write(dir.join("state.json"), "not json").unwrap();
        watch.later();
        assert!(watch.fill("a", (template, &dir), &blanks, None));
        let filled = watch.get("a").unwrap();
        assert_eq!(filled.text.as_deref(), Some("PR is merged"));
        assert!(filled.error.as_deref().unwrap().contains("not JSON"));
        let rows: Vec<String> = block(Some(filled), 60, &dir)
            .iter()
            .map(crate::transcript::seen)
            .collect();
        assert_eq!(rows[0].trim(), "◇ live");
        assert!(rows[1].contains("follows state.json"), "{rows:?}");
        assert!(rows.iter().any(|row| row.contains("PR is merged")));
        assert!(rows.iter().any(|row| row.contains("⟨line 1: not JSON")));
        assert!(watch.retain(|key| key != "a") && watch.get("a").is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_block_that_reads_the_tasks_fills_again_when_they_are_read_again() {
        let dir = std::env::temp_dir();
        let template = "{{ tasks() | length }} tasks";
        let mut watch = Watch::default();
        let mut blanks = blanks();
        watch.fill("t", (template, &dir), &blanks, None);
        blanks.tasks = Arc::new(vec![]);
        blanks.tasks_read += 1;
        watch.later();
        assert!(watch.fill("t", (template, &dir), &blanks, None));
        assert_eq!(watch.get("t").unwrap().text.as_deref(), Some("0 tasks"));
    }

    #[test]
    fn the_tool_host_answers_what_a_block_shows_and_refuses_what_never_fills() {
        let data = crate::template::Data {
            sources: Arc::new(vec!["ci".into()]),
            now: 1_000_000,
            ..Default::default()
        };
        let shown = answer("{{ 1 + 1 }} done", &data).unwrap();
        assert!(
            shown.starts_with("Shown live in the conversation, kept filled as it changes. At ")
                && shown.ends_with(" it shows:\n2 done"),
            "{shown}"
        );
        assert_eq!(answer("{% if %}", &data).unwrap_err().0, "live_template");
        assert_eq!(
            answer("{{ source('nope').latest }}", &data).unwrap_err().0,
            "live_template"
        );
        let later = answer("{{ 'x' | length }} {{ nope.state }}", &data).unwrap();
        assert!(later.starts_with("Shown live in the conversation; it does not fill here yet"));
        // No tasks here is not none: a block that reads them shows no count.
        for tasks in [
            "{{ tasks() | length }} tasks",
            "{{ task('calc.add').state }}",
        ] {
            let shown = answer(tasks, &data).unwrap();
            assert!(
                shown.contains("only the window knows") && !shown.contains("0 tasks"),
                "{shown}"
            );
        }
        // What it shows the agent is at most what a prompt keeps.
        let big = answer("{% for i in range(9000) %}0123456789{% endfor %}", &data).unwrap();
        assert!(big.len() < 33 * 1024 && big.contains("[cut: it filled to "));
    }
}
