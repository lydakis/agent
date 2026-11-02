//! The pane beside the conversation on the full screen: the margin, with
//! the agent's notes, and laid over it what the person opens to look at
//! instead of over the conversation: a file they clicked, as a preview,
//! `/status`, or a side chat. Its rows are a [`Screen`] of their own, so
//! they scroll, select, copy and follow links as the conversation's do.
use std::path::{Path, PathBuf};

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style, Stylize},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

use crate::screen::Screen;

/// The key that moves the keys between the conversation and the pane, and
/// in a window too narrow for both, which of them shows.
pub const SWITCH: &str = "ctrl+\\";
/// The narrowest the margin is shown by itself, beside a conversation
/// that has all it is written at.
pub const MARGIN: u16 = 40;
/// What the keys do in the margin.
pub const MARGIN_KEYS: &str = "esc close";
/// The key, and its hint, that opens a picture or a drawing bigger: in
/// the app the system opens such a file with, as large as it lets it be.
pub const BIGGER: &str = "o open bigger";
/// The most of a file read for its preview, and the most lines shown.
const MOST_BYTES: u64 = 4 << 20;
const MOST_LINES: usize = 9_000;
/// Rows kept above a line a link pointed at, so it reads in its place.
const ABOVE: usize = 3;

/// How wide the pane is in a window `width` columns wide, beside a
/// conversation written at most `prose` wide, whatever it shows: what the
/// conversation leaves when it keeps all it is written at, as the margin
/// takes, so opening a file or a card never moves it. A window with less
/// room than [`MARGIN`] beside it shows one at a time: all pane while the
/// keys are in it, [`SWITCH`] back to the conversation.
pub fn width(width: u16, prose: u16) -> u16 {
    margin_width(width, prose).unwrap_or(width)
}

/// How wide the margin is by itself in a window `width` columns wide,
/// beside a conversation written at most `prose` wide: what the
/// conversation leaves, when that is at least [`MARGIN`].
pub fn margin_width(width: u16, prose: u16) -> Option<u16> {
    let rest = width.checked_sub(prose + 1)?;
    (rest >= MARGIN).then_some(rest)
}

pub struct Pane {
    /// What the pane shows, as its first row says it.
    title: String,
    /// The file's address, as its link held it, opened outside Sonny
    /// when the title is clicked.
    link: Option<String>,
    /// The folder the file is in, which paths in it are read from.
    folder: Option<PathBuf>,
    pub screen: Screen,
    /// Where it was drawn last: its title row and its rows.
    pub area: Rect,
    /// A card's lines as last set, drawn anew from the app's own state
    /// (`/status`), rather than a file read once.
    card: Option<Vec<Line<'static>>>,
    /// What the keys do here, after the title while they go to it.
    keys: String,
    /// A program drawn here instead of rows: a side chat's Sonny.
    pub program: Option<crate::embed::Embedded>,
    /// A line kept on the last row under a card, however it is scrolled:
    /// `/background`'s keys, or what its last key did.
    pub foot: Option<Line<'static>>,
    /// Its rows are prose, read at the width the conversation is written
    /// at however wide the pane, its title over them as wide: Markdown,
    /// plain text, a tree, the margin and the cards. Code, pictures,
    /// diagrams, pages and the sky go as wide as the pane.
    pub prose: bool,
    /// What the pane showed before, as a browser keeps it, the latest
    /// last, each as it was left; and what it went back from.
    back: Vec<Pane>,
    forward: Vec<Pane>,
    /// A folder, drawn as a tree its folders open and close in.
    tree: Option<crate::tree::Tree>,
    width: usize,
    /// The margin's first row in place of a title, and its buttons.
    head: Option<(Line<'static>, crate::screen::Buttons)>,
    /// The marks the margin's lines were last set with.
    marks: Vec<(usize, crate::screen::Mark)>,
    /// It is the margin, not something opened over it.
    margin: bool,
    /// The margin is under it, which its title's arrow goes back to.
    pub over: bool,
    /// It has the window to itself, the conversation a key away, as a
    /// window too narrow for both shows one at a time.
    pub alone: bool,
    /// The columns the switch key's hint was last drawn at on its title,
    /// which a click on takes the keys over, as the key does.
    switch_at: Option<(u16, u16)>,
    /// The keys were here when it was last drawn: its switch hint went
    /// back to the conversation then, and took them over while not.
    focused: bool,
    /// A picture or a drawing it shows, which [`BIGGER`] opens outside
    /// Sonny; and where that hint was last drawn, which a click on does too.
    bigger: Option<PathBuf>,
    bigger_at: Option<(u16, u16)>,
    /// The agent whose Sonny the program is, which the title's button
    /// opens in the main view; and that button's columns as last drawn.
    pub agent: Option<String>,
    main_at: Option<(u16, u16)>,
    /// Where its name was last drawn, when a click on it goes to the
    /// agent's own tab in the multiplexer.
    name_at: Option<(u16, u16)>,
    tab: bool,
    /// A Markdown file's path and the width its rows were laid at, laid
    /// again at the width it is drawn at whenever that changes, so a
    /// table or a list is never wrapped into pieces as the pane narrows.
    laid: Option<(PathBuf, usize)>,
}

/// The button beside an agent's name that opens it in the main view: the
/// visual system's opens mark, its words in the keys beside it, as `←`
/// is back's.
const MAIN: &str = " ↗";
/// What the keys do on an agent's Sonny in the pane.
const AGENT_KEYS: &str = "ctrl+g main view · ctrl+\\ conversation";
/// The same, for one laid out in a multiplexer, whose tab its name and
/// Ctrl+T go to.
const LAID_OUT_KEYS: &str = "ctrl+g main view · ctrl+t its tab · ctrl+\\ conversation";

/// Ctrl+G, which opens the agent in the pane in the main view, as its
/// title's button does: no Sonny key takes it.
pub fn to_main(key: &ratatui::crossterm::event::KeyEvent) -> bool {
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};
    key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('g')
}

/// Ctrl+T, which goes to the agent's own tab in the multiplexer, as a
/// click on its name does.
pub fn to_tab(key: &ratatui::crossterm::event::KeyEvent) -> bool {
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};
    key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('t')
}

impl Pane {
    /// A file's preview, from a `file://` address as a link holds it,
    /// with a `#L…` line to show, at `width`. Paths are shown from
    /// `workspace` when they are in it.
    pub fn preview(url: &str, workspace: &Path, width: usize) -> Result<Self, String> {
        let (path, line) = file_url(url).ok_or_else(|| format!("{url} is no file"))?;
        // `docs/../a.md`, as a link between files may name it, is `a.md`.
        let path = path.components().fold(PathBuf::new(), |mut path, part| {
            match part {
                std::path::Component::ParentDir => _ = path.pop(),
                std::path::Component::CurDir => {}
                part => path.push(part),
            }
            path
        });
        let title = match path.strip_prefix(workspace) {
            Ok(inside) if !inside.as_os_str().is_empty() => inside.display().to_string(),
            _ => path.display().to_string(),
        };
        let mut screen = Screen::default();
        let tree = std::fs::metadata(&path)
            .is_ok_and(|meta| meta.is_dir())
            .then(|| crate::tree::Tree::new(&path));
        // A tree reads at the conversation's width, its marks near its names.
        let width = match tree {
            Some(_) => width.min(crate::measure()),
            None => width,
        };
        let (target, prose) = match &tree {
            Some(tree) => {
                for (line, url) in tree.lines(width) {
                    list(&mut screen, line, url);
                }
                (0, true)
            }
            None => preview(&path, width, line, &mut screen)?,
        };
        screen.show_row(target, ABOVE);
        let laid =
            (tree.is_none() && line.is_none() && is_markdown(&path)).then(|| (path.clone(), width));
        // Its escapes kept, as the path read back may hold `#` or `%`;
        // the line left off.
        let link = url.split('#').next().unwrap_or(url);
        // A pipe named `.svg` is no drawing: reading it could wait forever.
        let bigger = (path.is_file() && is_drawing(&path)).then(|| path.clone());
        Ok(Self {
            title,
            link: Some(link.to_owned()),
            folder: match path.is_dir() {
                true => Some(path.clone()),
                false => path.parent().map(Path::to_path_buf),
            },
            screen,
            area: Rect::default(),
            card: None,
            keys: match bigger {
                Some(_) => format!("{BIGGER} · esc close"),
                None => "esc close".into(),
            },
            program: None,
            foot: None,
            prose,
            back: Vec::new(),
            forward: Vec::new(),
            tree,
            width,
            head: None,
            marks: Vec::new(),
            margin: false,
            over: false,
            alone: false,
            switch_at: None,
            focused: false,
            bigger,
            bigger_at: None,
            agent: None,
            main_at: None,
            name_at: None,
            tab: false,
            laid,
        })
    }

    /// Shown in place of `before`, after it as a browser goes: what was
    /// gone back from left behind.
    pub fn after(mut self, mut before: Pane) -> Self {
        self.back = std::mem::take(&mut before.back);
        before.forward.clear();
        self.back.push(before);
        if self.back.len() > HISTORY {
            self.back.remove(0);
        }
        self
    }

    /// Whether there is something to go back (`back`) or forward to.
    pub fn can_go(&self, back: bool) -> bool {
        match back {
            true => !self.back.is_empty(),
            false => !self.forward.is_empty(),
        }
    }

    /// What the pane showed before (`back`), or went back from, as it was
    /// left, this kept to come back to; or this again, with nowhere to go.
    pub fn go(mut self, back: bool) -> Self {
        let Some(mut next) = (match back {
            true => self.back.pop(),
            false => self.forward.pop(),
        }) else {
            return self;
        };
        next.back = std::mem::take(&mut self.back);
        next.forward = std::mem::take(&mut self.forward);
        match back {
            true => next.forward.push(self),
            false => next.back.push(self),
        }
        next
    }

    /// A folder in its tree opened or closed, if `url` names one; the
    /// view kept where it was.
    pub fn toggle(&mut self, url: &str) -> bool {
        let Some(tree) = &mut self.tree else {
            return false;
        };
        let Some((path, _)) = file_url(url) else {
            return false;
        };
        if !tree.toggle(&path) {
            return false;
        }
        // A tree reads at the conversation's width, as it is drawn.
        let width = (self.area.width as usize).min(crate::measure());
        let back = self.screen.back();
        let before = self.screen.height(width);
        self.screen = Screen::default();
        for (line, url) in tree.lines(self.width) {
            list(&mut self.screen, line, url);
        }
        // Rows open and close under the view: its top stays put.
        let after = self.screen.height(width);
        self.screen.hold((back + after).saturating_sub(before));
        true
    }

    /// Whether `(column, row)` is on the title's ‹ (true) or › (false),
    /// while it goes somewhere: back is what was shown before, or the
    /// margin under it, the first step back.
    pub fn on_arrow(&self, column: u16, row: u16) -> Option<bool> {
        if row != self.area.y || !self.arrows() {
            return None;
        }
        match column.checked_sub(self.area.x)? {
            1 if self.goes_back() => Some(true),
            3 if !self.forward.is_empty() => Some(false),
            _ => None,
        }
    }

    /// Whether back goes somewhere: to what was shown before, or to the
    /// margin under it.
    pub fn goes_back(&self) -> bool {
        self.over || !self.back.is_empty()
    }

    /// Whether the title shows ‹ and ›: while either goes somewhere.
    fn arrows(&self) -> bool {
        self.goes_back() || !self.forward.is_empty()
    }

    /// A card titled `title` whose lines the app sets as they change,
    /// as `/status` counts on; `keys` says what the keys do on it.
    pub fn card(title: &str, keys: &str) -> Self {
        Self {
            title: title.to_owned(),
            link: None,
            folder: None,
            screen: Screen::default(),
            area: Rect::default(),
            card: Some(Vec::new()),
            keys: keys.to_owned(),
            program: None,
            foot: None,
            prose: true,
            back: Vec::new(),
            forward: Vec::new(),
            tree: None,
            width: 0,
            head: None,
            marks: Vec::new(),
            margin: false,
            over: false,
            alone: false,
            switch_at: None,
            focused: false,
            bigger: None,
            bigger_at: None,
            agent: None,
            main_at: None,
            name_at: None,
            tab: false,
            laid: None,
        }
    }

    /// `program` drawn in the pane under `title`, every key going to it
    /// but Ctrl+\, which goes back to the conversation.
    pub fn program(title: &str, program: crate::embed::Embedded) -> Self {
        Self {
            title: title.to_owned(),
            link: None,
            folder: None,
            screen: Screen::default(),
            area: Rect::default(),
            card: None,
            keys: "ctrl+\\ conversation".into(),
            program: Some(program),
            foot: None,
            prose: false,
            back: Vec::new(),
            forward: Vec::new(),
            tree: None,
            width: 0,
            head: None,
            marks: Vec::new(),
            margin: false,
            over: false,
            alone: false,
            switch_at: None,
            focused: false,
            bigger: None,
            bigger_at: None,
            agent: None,
            main_at: None,
            name_at: None,
            tab: false,
            laid: None,
        }
    }

    /// The program is `agent`'s Sonny: its title's button and Ctrl+G open
    /// it in the main view, and, when it is laid out in a multiplexer
    /// (`tab`), its name and Ctrl+T go to its own tab there.
    pub fn as_agent(&mut self, agent: &str, tab: bool) {
        self.agent = Some(agent.to_owned());
        self.tab = tab;
        self.keys = match tab {
            true => LAID_OUT_KEYS,
            false => AGENT_KEYS,
        }
        .to_owned();
    }

    /// Whether Ctrl+T goes to the agent's tab.
    pub fn has_tab(&self) -> bool {
        self.tab && self.agent.is_some()
    }

    /// Where what the pane holds starts, under its title: the program's
    /// first cell.
    pub fn origin(&self) -> (u16, u16) {
        (self.area.x + 1, self.area.y + 2)
    }

    /// The margin: the agent's notes, under a glance line.
    pub fn margin() -> Self {
        Self {
            margin: true,
            ..Self::card("margin", MARGIN_KEYS)
        }
    }

    /// Whether this is a card the app draws, not a file or the margin.
    pub fn is_card(&self) -> bool {
        self.card.is_some() && !self.margin
    }

    /// Whether this is a card, or one is back or forward from here: what
    /// keeps `/status` read for going back to it.
    pub fn keeps_card(&self) -> bool {
        self.is_card() || (self.back.iter().chain(&self.forward)).any(Pane::is_card)
    }

    pub fn is_margin(&self) -> bool {
        self.margin
    }

    /// The picture or drawing [`BIGGER`] opens, if it shows one.
    pub fn bigger(&self) -> Option<&Path> {
        self.bigger.as_deref()
    }

    /// What its first row names it.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// The margin's lines, with their tags and buttons, put in only when
    /// they changed, as [`Pane::set_card`] puts a card's; and its first row.
    pub fn set_margin(
        &mut self,
        lines: Vec<Line<'static>>,
        marks: Vec<(usize, crate::screen::Mark)>,
        head: (Line<'static>, crate::screen::Buttons),
    ) {
        self.head = Some(head);
        // The same rows for another note still point at the other note.
        if self.card.as_ref() != Some(&lines) || self.marks != marks {
            self.screen.replace_marked(lines.clone(), marks.clone());
            self.card = Some(lines);
            self.marks = marks;
        }
    }

    /// The button under `column` on the title row, if any: the margin's
    /// glance, or the arrow back to the margin.
    pub fn head_button(&self, column: u16) -> Option<String> {
        let at = column.checked_sub(self.area.x)?;
        if let (Some(agent), Some((from, to))) = (&self.agent, self.main_at)
            && (from..to).contains(&column)
        {
            return Some(format!("{}main/{agent}", crate::screen::BUTTON));
        }
        if let (Some(agent), Some((from, to))) = (&self.agent, self.name_at)
            && (from..to).contains(&column)
        {
            return Some(format!("{}tab/{agent}", crate::screen::BUTTON));
        }
        if let Some((from, to)) = self.switch_at
            && (from..to).contains(&column)
        {
            // Unfocused, the hint says `tab focus`: a click there gives
            // the pane the keys, as a click anywhere on it already has.
            let what = match self.focused {
                true => "switch",
                false => "focus",
            };
            return Some(format!("{}{what}", crate::screen::BUTTON));
        }
        if let Some((from, to)) = self.bigger_at
            && (from..to).contains(&column)
        {
            return Some(format!("{}bigger", crate::screen::BUTTON));
        }
        let (_, buttons) = self.head.as_ref()?;
        (buttons.iter())
            .find(|(from, to, _)| (*from..*to).contains(&at))
            .map(|(.., url)| url.clone())
    }

    /// A card's lines, put in only when they changed, so a selection in
    /// them lasts while they do not, and the view stays where it was.
    pub fn set_card(&mut self, lines: Vec<Line<'static>>, keys: &str) {
        self.set_card_marked(lines, Vec::new(), keys);
    }

    /// [`Pane::set_card`], with links on its rows, such as `/status`'s
    /// agents, each of which a click opens.
    pub fn set_card_marked(
        &mut self,
        lines: Vec<Line<'static>>,
        marks: Vec<(usize, crate::screen::Mark)>,
        keys: &str,
    ) {
        if self.keys != keys {
            self.keys = keys.to_owned();
        }
        if self.card.as_ref() != Some(&lines) || self.marks != marks {
            self.screen.replace_marked(lines.clone(), marks.clone());
            self.card = Some(lines);
            self.marks = marks;
        }
    }

    /// The folder of the file shown, if a file is.
    pub fn folder(&self) -> Option<&Path> {
        self.folder.as_deref()
    }

    /// The file's address, to open it outside, if it shows one.
    pub fn url(&self) -> Option<String> {
        self.link.clone()
    }

    /// Whether `(column, row)` is on the title, which opens the file.
    pub fn on_title(&self, column: u16, row: u16) -> bool {
        row == self.area.y && (self.area.x..self.area.right()).contains(&column)
    }

    /// The pane in `area`: its title over its rows, the title bright
    /// while keys go to the pane, else naming `switch`, the key that
    /// sends them there. `file` is where a word that may be a path opens,
    /// as in the conversation. Where a program's cursor is, when it shows
    /// one.
    pub fn render(
        &mut self,
        area: Rect,
        buf: &mut Buffer,
        focused: bool,
        switch: &str,
        file: impl Fn(&str) -> Option<String>,
    ) -> Option<(u16, u16)> {
        // Prose and its title read at the conversation's measure, a column
        // in from the line between; what is left of a wide pane stays
        // empty on the right, as beside the conversation alone.
        let measure = crate::measure() as u16 + 1;
        let area = match self.prose && self.program.is_none() {
            true => Rect {
                width: area.width.min(measure),
                ..area
            },
            false => area,
        };
        self.area = area;
        if area.height == 0 {
            return None;
        }
        // Alone, the way back to the conversation comes first after back,
        // the last of the keys to go when the title runs out of room.
        let own = match self.alone && !self.keys.contains(SWITCH) {
            true => format!("{SWITCH} conversation · {}", self.keys),
            false => self.keys.clone(),
        };
        let mut keys = match focused {
            true if self.goes_back() => format!("  ← back · {own} "),
            true => format!("  {own} "),
            false => format!("  {switch} focus "),
        };
        let mut style = Style::new();
        // A file's name is a link to it. A laid out agent's goes to its
        // tab, so it is drawn as any agent's name that opens it is: in the
        // agents' color, never underlined.
        if self.link.is_some() {
            style = style.add_modifier(Modifier::UNDERLINED);
        } else if self.tab {
            style = style.fg(ratatui::style::Color::Blue);
        }
        if !focused {
            style = style.add_modifier(Modifier::DIM);
        } else {
            style = style.add_modifier(Modifier::BOLD);
        }
        // ‹ and › go back and forward as a browser's do, the margin under
        // what is open the first step back, while either goes somewhere.
        let mut head = vec![Span::raw(" ")];
        if self.arrows() {
            let arrow = |text, there: bool| match there {
                true => Span::raw(text),
                false => Span::raw(text).dark_gray(),
            };
            head.push(arrow("‹", self.goes_back()));
            head.push(Span::raw(" "));
            head.push(arrow("›", !self.forward.is_empty()));
            head.push(Span::raw(" "));
        }
        // A name keeps its room: the keys before esc go first, from the
        // last, as the footer's least urgent do; esc, named last, stays.
        if self.head.is_none() {
            let button = if self.agent.is_some() {
                MAIN.width()
            } else {
                0
            };
            let name =
                head.iter().map(|s| s.content.width()).sum::<usize>() + self.title.width() + button;
            keys = fit_keys(&keys, (area.width as usize).saturating_sub(name));
        }
        // The switch key's hint is a button too: where it sits, as the
        // keys end at the title's right edge.
        let from = area.x + (area.width as usize).saturating_sub(keys.width()) as u16;
        let said = match focused {
            true => SWITCH,
            false => switch,
        };
        self.switch_at = hint_at(&keys, said).map(|(a, b)| (from + a, from + b));
        self.bigger_at = (focused && self.bigger.is_some())
            .then(|| hint_at(&keys, BIGGER))
            .flatten()
            .map(|(a, b)| (from + a, from + b));
        self.focused = focused;
        // Keys in their one grammar while they go here, else dim.
        let key_spans = match focused {
            true => crate::hint::spans(&keys),
            false => vec![Span::raw(keys.clone()).dim()],
        };
        let head = match &self.head {
            // The margin's glance in place of a title, its keys after.
            Some((glance, _)) => {
                let room = (area.width as usize).saturating_sub(keys.width() + 1);
                let cut = crate::transcript::cut(&glance.to_string(), room);
                let mut spans = match cut.width() < glance.width() {
                    true => vec![Span::raw(cut)],
                    false => glance.spans.clone(),
                };
                let gap =
                    (area.width as usize).saturating_sub(glance.width().min(room) + keys.width());
                spans.push(Span::raw(" ".repeat(gap)));
                spans.extend(key_spans);
                Line::from(spans)
            }
            None => {
                // An agent's Sonny opens in the main view from its title,
                // where there is room for the button beside the name.
                let button = match &self.agent {
                    Some(_) => MAIN,
                    None => "",
                };
                let used: usize = head.iter().map(|s| s.content.width()).sum();
                let room = (area.width as usize).saturating_sub(used + keys.width());
                let button = match self.title.width() + button.width() <= room {
                    true => button,
                    false => "",
                };
                let title = crate::transcript::cut(&self.title, room - button.width());
                let gap = (area.width as usize)
                    .saturating_sub(used + title.width() + button.width() + keys.width());
                let at = area.x + (used + title.width() + 1) as u16;
                let name = area.x + used as u16;
                self.name_at = self.tab.then_some((name, name + title.width() as u16));
                head.push(Span::styled(title, style));
                self.main_at = None;
                if !button.is_empty() {
                    self.main_at = Some((at, at + 1));
                    head.push(Span::raw(button).fg(ratatui::style::Color::Cyan));
                }
                head.push(Span::raw(" ".repeat(gap)));
                head.extend(key_spans);
                Line::from(head)
            }
        };
        buf.set_line(area.x, area.y, &head, area.width);
        // A row between the name and what it holds, and above the foot.
        let mut rows = Rect {
            y: area.y + 2,
            height: area.height.saturating_sub(2),
            ..area
        };
        if let Some(foot) = &self.foot
            && rows.height > 2
        {
            rows.height -= 2;
            buf.set_line(area.x, rows.bottom() + 1, &inset(foot), area.width);
        }
        match &mut self.program {
            // A column off the line between, as the title is.
            Some(program) => {
                let rows = Rect {
                    x: rows.x + 1,
                    width: rows.width.saturating_sub(1),
                    ..rows
                };
                program.render(rows, buf)
            }
            None => {
                // Prose reads at the width the conversation is written at.
                let rows = match self.prose {
                    true => Rect {
                        width: rows.width.min(crate::measure() as u16),
                        ..rows
                    },
                    false => rows,
                };
                self.lay(rows);
                self.screen.render(rows, buf, file);
                None
            }
        }
    }
}

impl Pane {
    /// A Markdown file laid again at the width of `rows` when it was laid
    /// at another, read anew, the view kept as far through it as it was.
    fn lay(&mut self, rows: Rect) {
        let width = rows.width as usize;
        let Some((path, laid)) = &self.laid else {
            return;
        };
        if *laid == width || width == 0 {
            return;
        }
        let path = path.clone();
        let height = rows.height as usize;
        // Not drawn yet, the view asked for (the file's top, at opening)
        // is where the new rows open; drawn, they open as far through.
        let going_to = self.screen.going_to();
        let before = self.screen.height(*laid).saturating_sub(height).max(1);
        let through = 1.0 - self.screen.back().min(before) as f64 / before as f64;
        let mut screen = Screen::default();
        if preview(&path, width, None, &mut screen).is_err() {
            // Gone or unreadable now: what was read stays as it was.
            self.laid = None;
            return;
        }
        match going_to {
            Some((row, above)) => screen.show_row(row, above),
            None => {
                let after = screen.height(width).saturating_sub(height);
                screen.hold(((1.0 - through) * after as f64).round() as usize);
            }
        }
        self.screen = screen;
        self.laid = Some((path, width));
    }
}

/// Whether `path` is read as Markdown, whose tables and lists are laid
/// out for the width they are drawn at.
/// A picture, or a file Sonny draws: what [`BIGGER`] opens.
fn is_drawing(path: &Path) -> bool {
    let extension = (path.extension().and_then(|e| e.to_str()))
        .unwrap_or_default()
        .to_lowercase();
    crate::image::bigger_kind(&extension).is_some()
}

fn is_markdown(path: &Path) -> bool {
    let extension = (path.extension().and_then(|e| e.to_str()))
        .unwrap_or_default()
        .to_lowercase();
    matches!(extension.as_str(), "md" | "markdown" | "mdx")
}

/// The columns within `keys`, as the title holds them, of the hint that
/// starts with `said`, to its ` · `: the switch key's, `ctrl+\
/// conversation` while the keys are here and `tab focus` while not, or
/// [`BIGGER`]'s.
fn hint_at(keys: &str, said: &str) -> Option<(u16, u16)> {
    let start = keys.find(said)?;
    let end = keys[start..]
        .find(" · ")
        .map_or(keys.trim_end().len(), |at| start + at);
    Some((keys[..start].width() as u16, keys[..end].width() as u16))
}

/// `keys`, padded as the title holds them, in at most `room` columns. "← back"
/// goes first, since the ‹ beside the name already says it; then the pairs
/// before the last, from the one before it, so esc, which every view names
/// last, stays while anything does.
fn fit_keys(keys: &str, room: usize) -> String {
    let mut pairs: Vec<&str> = keys.trim().split(" · ").collect();
    let said = |pairs: &[&str]| format!("  {} ", pairs.join(" · "));
    while said(&pairs).width() > room && pairs.len() > 1 {
        let at = match pairs.iter().position(|pair| *pair == "← back") {
            Some(at) if at + 1 < pairs.len() => at,
            _ => pairs.len() - 2,
        };
        pairs.remove(at);
    }
    said(&pairs)
}

/// The path and line a `file://` address names, its `%XX` escapes read.
pub fn file_url(url: &str) -> Option<(PathBuf, Option<usize>)> {
    let rest = url.strip_prefix("file://")?;
    let (path, line) = match rest.split_once("#L") {
        Some((path, line)) => (path, line.parse().ok()),
        None => (rest.split('#').next().unwrap_or(rest), None),
    };
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let hex = bytes
            .get(at + 1..at + 3)
            .and_then(|h| std::str::from_utf8(h).ok());
        match (bytes[at], hex.and_then(|h| u8::from_str_radix(h, 16).ok())) {
            (b'%', Some(byte)) => {
                out.push(byte);
                at += 3;
            }
            (byte, _) => {
                out.push(byte);
                at += 1;
            }
        }
    }
    Some((PathBuf::from(String::from_utf8(out).ok()?), line))
}

/// `path` written into `screen` at `width`: a picture drawn, Markdown
/// as replies read, code in its colors with its line numbers. The row to show first: that of `line`, if any; and
/// whether its rows are prose, to be read no wider than the conversation.
fn preview(
    path: &Path,
    width: usize,
    line: Option<usize>,
    screen: &mut Screen,
) -> Result<(usize, bool), String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    // A pipe, socket or device may never end, or never start.
    if !meta.is_file() {
        screen.push(vec![note(
            "not a file a preview reads: a pipe, socket or device",
        )]);
        return Ok((0, false));
    }
    let size = meta.len();
    let extension = (path.extension().and_then(|e| e.to_str()))
        .unwrap_or_default()
        .to_lowercase();
    if matches!(extension.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp") {
        let shown = match crate::term::graphics() {
            true => crate::image::file(path, width, crate::image::tall()).map(crate::image::show),
            false => Err("this terminal draws no pictures (Kitty and Ghostty do)".into()),
        };
        match shown {
            Ok(rows) => screen.push(rows),
            Err(error) => screen.push(vec![note(&format!(
                "a picture, {}: {error}; click its name to open it",
                bytes_said(size)
            ))]),
        }
        return Ok((0, false));
    }
    let mut bytes = Vec::new();
    use std::io::Read;
    std::fs::File::open(path)
        .and_then(|f| f.take(MOST_BYTES).read_to_end(&mut bytes))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let text = match String::from_utf8(bytes) {
        Ok(text) if !text.contains('\0') => text,
        // Cut in the middle of a character at the end of what was read.
        Err(error) if size > MOST_BYTES && error.utf8_error().error_len().is_none() => {
            let valid = error.utf8_error().valid_up_to();
            let mut bytes = error.into_bytes();
            bytes.truncate(valid);
            String::from_utf8(bytes).unwrap_or_default()
        }
        _ => {
            screen.push(vec![note(&format!(
                "a binary file, {}; click its name to open it",
                bytes_said(size)
            ))]);
            return Ok((0, false));
        }
    };
    // Drawn only where pictures can be, as tall as the pane; elsewhere,
    // or when it cannot be drawn, its source, and why.
    if line.is_none() && crate::term::graphics() {
        let drawn =
            match extension.as_str() {
                "svg" => Some(crate::image::render_in(
                    &text,
                    width,
                    crate::image::tall(),
                    true,
                )),
                "mmd" | "mermaid" => Some(crate::mermaid::svg(&text).and_then(|svg| {
                    crate::image::render_in(&svg, width, crate::image::tall(), true)
                })),
                "html" | "htm" => Some(
                    crate::html::render(path, crate::image::page_width(width))
                        .and_then(|png| crate::image::page_in(&png, width, crate::image::tall())),
                ),
                _ => None,
            };
        match drawn.map(|drawn| drawn.map(crate::image::show)) {
            Some(Ok(rows)) => {
                screen.push(rows);
                return Ok((0, false));
            }
            Some(Err(error)) => screen.push(vec![
                note(&format!("could not draw it: {error}")),
                Line::default(),
            ]),
            None => {}
        }
    }
    let lines: Vec<&str> = text.lines().collect();
    let shown = lines.len().min(MOST_LINES);
    let target = line.map(|line| line.saturating_sub(1).min(shown.saturating_sub(1)));
    let mut first_row = 0;
    let prose;
    if matches!(extension.as_str(), "md" | "markdown" | "mdx") && line.is_none() {
        let folder = path.parent().unwrap_or(Path::new("/"));
        // A document reads at the conversation's width, its pictures too.
        prose = true;
        let mut markdown = crate::transcript::Markdown::document(folder);
        // From the column the title starts at, as every row in the pane.
        let leads = (Line::from(" "), Line::from(" "));
        let text = lines[..shown].join("\n");
        let rows = crate::transcript::markdown(&mut markdown, &text, width, leads);
        screen.push(rows);
    } else {
        let mut code = crate::highlight::Code::file(path, lines.first().unwrap_or(&""));
        // Text no language colors is read as prose.
        prose = code.is_none();
        let digits = shown.max(1).to_string().len();
        for (at, text) in lines[..shown].iter().enumerate() {
            let text = text.replace('\t', "    ");
            let number = format!("{:>digits$}  ", at + 1);
            let number = match Some(at) == target {
                true => Span::raw(number).reversed().bold(),
                false => Span::raw(number).dim(),
            };
            let mut spans = vec![Span::raw(" "), number];
            match &mut code {
                Some(code) => spans.extend(code.line(&text)),
                None => spans.push(Span::raw(text)),
            }
            let rest = Line::from(" ".repeat(digits + 3));
            screen.push_hanging(Line::from(spans), rest);
        }
        first_row = target.unwrap_or(0);
    }
    if lines.len() > shown || size > MOST_BYTES {
        screen.push(vec![
            Line::default(),
            note("… the rest is past what a preview shows; click its name to open it"),
        ]);
    }
    Ok((first_row, prose))
}

/// The most the pane keeps to go back to.
const HISTORY: usize = 50;

/// A row of a list that opens `url` when there is one.
fn list(screen: &mut Screen, line: Line<'static>, url: Option<String>) {
    match url {
        Some(url) => screen.push_linked(line, url),
        None => screen.push(vec![line]),
    }
}

/// `line` with the pane's one column of inset before it, whatever its own
/// lead: every row in the pane starts where the title's does.
fn inset(line: &Line<'static>) -> Line<'static> {
    let mut spans = line.spans.clone();
    while let Some(first) = spans.first_mut() {
        let trimmed = first.content.trim_start();
        if trimmed.is_empty() {
            spans.remove(0);
            continue;
        }
        if trimmed.len() != first.content.len() {
            first.content = trimmed.to_owned().into();
        }
        break;
    }
    spans.insert(0, Span::raw(" "));
    Line::from(spans).style(line.style)
}

fn note(text: &str) -> Line<'static> {
    Line::from(format!(" {text}")).dim()
}

fn bytes_said(size: u64) -> String {
    match size {
        0..1024 => format!("{size} bytes"),
        1024..1_048_576 => format!("{} KB", size / 1024),
        _ => format!("{:.1} MB", size as f64 / 1_048_576.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(pane: &mut Pane, width: u16, height: u16) -> Vec<String> {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        pane.render(area, &mut buf, true, "tab", |_| None);
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn an_agents_title_opens_it_in_the_main_view_where_it_fits() {
        let mut pane = Pane::card("shop.fix", "esc close");
        pane.as_agent("shop.fix", false);
        let head = rows(&mut pane, 60, 3).remove(0);
        assert!(head.starts_with(" shop.fix ↗ "), "{head}");
        assert!(
            head.ends_with("ctrl+g main view · ctrl+\\ conversation"),
            "{head}"
        );
        let column = " shop.fix ".width() as u16;
        assert_eq!(
            pane.head_button(column).as_deref(),
            Some("sonny:main/shop.fix")
        );
        assert_eq!(pane.head_button(1), None);
        assert_eq!(pane.head_button(column + 1), None);
        // Too narrow for both: the name stays, the button goes.
        let head = rows(&mut pane, 24, 3).remove(0);
        assert!(!head.contains('↗'), "{head}");
        assert_ne!(
            pane.head_button(column).as_deref(),
            Some("sonny:main/shop.fix")
        );
    }

    #[test]
    fn a_laid_out_agents_name_goes_to_its_tab() {
        let mut pane = Pane::card("shop.fix", "esc close");
        pane.as_agent("shop.fix", true);
        assert!(pane.has_tab());
        let head = rows(&mut pane, 72, 3).remove(0);
        assert!(head.starts_with(" shop.fix ↗ "), "{head}");
        assert!(
            head.ends_with("ctrl+g main view · ctrl+t its tab · ctrl+\\ conversation"),
            "{head}"
        );
        let name = " shop.fix".width() as u16;
        assert_eq!(pane.head_button(1).as_deref(), Some("sonny:tab/shop.fix"));
        assert_eq!(
            pane.head_button(name - 1).as_deref(),
            Some("sonny:tab/shop.fix")
        );
        assert_eq!(pane.head_button(name), None);
        assert_eq!(
            pane.head_button(name + 1).as_deref(),
            Some("sonny:main/shop.fix")
        );
        // Narrower, the keys before the last go first, before the name is cut.
        let head = rows(&mut pane, 56, 3).remove(0);
        assert!(head.starts_with(" shop.fix ↗ "), "{head}");
        assert!(
            head.ends_with("ctrl+g main view · ctrl+\\ conversation"),
            "{head}"
        );
        let head = rows(&mut pane, 36, 3).remove(0);
        assert!(head.starts_with(" shop.fix ↗ "), "{head}");
        assert!(head.ends_with("  ctrl+\\ conversation"), "{head}");
        // Not laid out: the name is only a name.
        pane.as_agent("shop.fix", false);
        rows(&mut pane, 72, 3);
        assert!(!pane.has_tab());
        assert_eq!(pane.head_button(1), None);
    }

    #[test]
    fn a_pane_alone_says_how_to_get_back_to_the_conversation() {
        let mut pane = Pane::card("status", "↑↓ pick · esc close");
        pane.alone = true;
        let head = rows(&mut pane, 70, 3).remove(0);
        assert!(
            head.ends_with("ctrl+\\ conversation · ↑↓ pick · esc close"),
            "{head}"
        );
        // Narrow, the pick goes before the way back does.
        let head = rows(&mut pane, 44, 3).remove(0);
        assert!(head.ends_with("ctrl+\\ conversation · esc close"), "{head}");
        // The hint is a button that does what the key does.
        let column = pane.area.x + 43 - "ctrl+\\ conversation · esc close".width() as u16;
        assert_eq!(
            pane.head_button(column).as_deref(),
            Some(&*format!("{}switch", crate::screen::BUTTON))
        );
        // Beside the conversation it needs no saying.
        pane.alone = false;
        let head = rows(&mut pane, 70, 3).remove(0);
        assert!(!head.contains("conversation"), "{head}");
        // Unfocused, its `tab focus` hint gives the pane the keys, never
        // takes them away, though a click on the pane gave them first.
        let mut buf = Buffer::empty(Rect::new(0, 0, 70, 3));
        pane.render(Rect::new(0, 0, 70, 3), &mut buf, false, "tab", |_| None);
        let column = 70 - "tab focus ".width() as u16;
        assert_eq!(
            pane.head_button(column).as_deref(),
            Some(&*format!("{}focus", crate::screen::BUTTON))
        );
    }

    #[test]
    fn a_cards_foot_stays_on_the_last_row_however_it_is_scrolled() {
        let mut pane = Pane::card("background", "esc close");
        let lines = (0..20).map(|i| Line::from(format!("row {i}"))).collect();
        pane.set_card(lines, "esc close");
        pane.foot = Some(Line::from("d again removes nightly"));
        let shown = rows(&mut pane, 30, 8);
        assert_eq!(shown[0], " background         esc close");
        assert_eq!(shown[2], "row 0");
        assert_eq!(shown[5], "row 3");
        assert_eq!(shown[6], "");
        assert_eq!(shown[7], " d again removes nightly");
        pane.screen.latest();
        let shown = rows(&mut pane, 30, 8);
        assert_eq!(shown[5], "row 19");
        assert_eq!(shown[7], " d again removes nightly");
    }

    #[test]
    fn the_pane_takes_what_the_conversation_leaves_or_the_window() {
        // The conversation keeps its 100 columns, as beside the margin,
        // a column kept for the line between.
        assert_eq!(width(260, 100), 159);
        assert_eq!(width(161, 100), 60);
        assert_eq!(width(141, 100), MARGIN);
        assert_eq!(width(141, 100), margin_width(141, 100).unwrap());
        // Less than the margin's room: one at a time, all of it.
        assert_eq!(width(140, 100), 140);
        assert_eq!(width(100, 100), 100);
    }

    #[test]
    fn an_address_reads_back_as_its_path_and_line() {
        let (path, line) = file_url("file:///w/a%20b.rs#L12").unwrap();
        assert_eq!(path, PathBuf::from("/w/a b.rs"));
        assert_eq!(line, Some(12));
        assert_eq!(file_url("file:///w/c.rs").unwrap().1, None);
        // The title opens the file by the address as the link wrote it.
        let dir = std::env::temp_dir().join(format!("sonny-url-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a#1%20.rs"), "x\n").unwrap();
        let url = format!("file://{}/a%231%2520.rs", dir.display());
        let pane = Pane::preview(&format!("{url}#L1"), &dir, 40).unwrap();
        assert_eq!(pane.url(), Some(url));
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(file_url("https://x").is_none());
    }

    #[test]
    fn a_picture_or_a_drawing_opens_bigger_from_its_title() {
        let dir = std::env::temp_dir().join(format!("sonny-pane-bigger-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("flow.mmd");
        std::fs::write(&file, "flowchart LR\n  a --> b\n").unwrap();
        let mut pane = Pane::preview(&format!("file://{}", file.display()), &dir, 60).unwrap();
        assert_eq!(pane.bigger(), Some(file.as_path()));
        let head = rows(&mut pane, 60, 3).remove(0);
        assert!(head.ends_with("o open bigger · esc close"), "{head}");
        // Its hint is a button, as the switch key's is.
        let at = head.find("open bigger").unwrap();
        let column = head[..at].width() as u16;
        assert_eq!(
            pane.head_button(column).as_deref(),
            Some(&*format!("{}bigger", crate::screen::BUTTON))
        );
        // Unfocused, its keys are not shown, and nothing there is a button.
        let mut buf = Buffer::empty(Rect::new(0, 0, 60, 3));
        pane.render(Rect::new(0, 0, 60, 3), &mut buf, false, "tab", |_| None);
        assert_eq!(pane.head_button(column), None);
        // Text is not a drawing.
        let notes = dir.join("notes.md");
        std::fs::write(&notes, "# Notes\n").unwrap();
        let pane = Pane::preview(&format!("file://{}", notes.display()), &dir, 60).unwrap();
        assert_eq!(pane.bigger(), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn code_shows_numbered_from_the_line_asked_for() {
        let dir = std::env::temp_dir().join(format!("sonny-pane-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.rs");
        let source: String = (1..=40).map(|n| format!("let x{n} = {n};\n")).collect();
        std::fs::write(&file, source).unwrap();
        let url = format!("file://{}#L20", file.display());
        let mut pane = Pane::preview(&url, &dir, 40).unwrap();
        let shown = rows(&mut pane, 40, 7);
        assert!(shown[0].starts_with(" a.rs"), "{shown:?}");
        assert!(shown[0].ends_with("esc close"), "{shown:?}");
        // Three rows above the line linked to, so it reads in place.
        assert_eq!(shown[2], " 17  let x17 = 17;");
        assert_eq!(shown[5], " 20  let x20 = 20;");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_line_asked_for_shows_under_a_long_line_above() {
        let dir = std::env::temp_dir().join(format!("sonny-above-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        let long = "word ".repeat(40);
        let source: String = (1..=30)
            .map(|n| match n {
                19 => format!("{long}\n"),
                n => format!("line {n}\n"),
            })
            .collect();
        std::fs::write(&file, source).unwrap();
        let url = format!("file://{}#L20", file.display());
        let mut pane = Pane::preview(&url, &dir, 30).unwrap();
        let shown = rows(&mut pane, 30, 8);
        // Three drawn rows above it, however many lines they are.
        assert_eq!(shown[5], " 20  line 20", "{shown:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_pipe_is_not_read() {
        let dir = std::env::temp_dir().join(format!("sonny-fifo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fifo = dir.join("pipe.svg");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        if made.is_ok_and(|s| s.success()) {
            // Opening it would wait for a writer that never comes.
            let mut pane = Pane::preview(&format!("file://{}", fifo.display()), &dir, 60).unwrap();
            let shown = rows(&mut pane, 60, 3);
            assert!(shown[2].contains("not a file a preview reads"), "{shown:?}");
            // Nor is it opened bigger, which would read it.
            assert_eq!(pane.bigger(), None);
            assert!(!shown[0].contains("open bigger"), "{shown:?}");
            assert!(crate::image::bigger(&fifo).is_err());
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_folders_entries_open_whatever_their_names() {
        let dir = std::env::temp_dir().join(format!("sonny-dir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a b.rs"), "x\n").unwrap();
        let mut pane = Pane::preview(&format!("file://{}", dir.display()), &dir, 80).unwrap();
        let area = Rect::new(0, 0, 80, 4);
        let mut buf = Buffer::empty(area);
        pane.render(area, &mut buf, true, "tab", |_| None);
        use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let ev = |kind| MouseEvent {
            kind,
            column: 117,
            row: 4,
            modifiers: KeyModifiers::NONE,
        };
        println!(
            "down {:?}",
            pane.screen
                .mouse(ev(MouseEventKind::Down(MouseButton::Left)))
                .is_some()
        );
        println!(
            "up {:?}",
            pane.screen
                .mouse(ev(MouseEventKind::Up(MouseButton::Left)))
                .map(|a| match a {
                    crate::screen::Action::Open(u) => u,
                    _ => "copy".into(),
                })
        );
        let cells = pane.screen.link_cells(&buf);
        let url = format!("file://{}/a%20b.rs", dir.display());
        // One link, over the whole name, spaces and all.
        assert!(cells.iter().all(|(_, y, _)| *y == 2));
        assert!(
            cells[0].2.symbol().contains(&url),
            "{:?}",
            cells[0].2.symbol()
        );
        let row: String = (0..80).map(|x| buf[(x, 2)].symbol()).collect();
        assert_eq!(cells.len(), row.trim().width());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_pane_goes_back_and_forward_as_a_browser_does() {
        let dir = std::env::temp_dir().join(format!("sonny-back-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("a.md"), "# A\n").unwrap();
        std::fs::write(dir.join("src/b.rs"), "fn b() {}\n").unwrap();
        let url = |name: &str| format!("file://{}", dir.join(name).display());
        let mut folder = Pane::preview(&url(""), &dir, 60).unwrap();
        // A folder opened in its tree stays open when it is come back to.
        assert!(folder.toggle(&url("src")));
        let a = Pane::preview(&url("a.md"), &dir, 60).unwrap().after(folder);
        let b = Pane::preview(&url("src/b.rs"), &dir, 60).unwrap().after(a);
        assert!(b.can_go(true) && !b.can_go(false));
        let mut shown = b.go(true).go(true);
        assert!(!shown.can_go(true) && shown.can_go(false));
        let shown_rows = rows(&mut shown, 60, 6);
        assert!(
            shown_rows[2..5].contains(&" ▾ src/".to_owned()),
            "{shown_rows:?}"
        );
        assert!(
            shown_rows[2..5].contains(&" │   b.rs".to_owned()),
            "{shown_rows:?}"
        );
        // The title's arrows: ‹ dim and dead with nowhere back to go, › forward.
        assert!(shown_rows[0].starts_with(" ‹ › "), "{shown_rows:?}");
        assert_eq!(shown.on_arrow(1, 0), None);
        assert_eq!(shown.on_arrow(3, 0), Some(false));
        assert_eq!(shown.on_arrow(5, 0), None);
        let mut a = shown.go(false);
        assert_eq!(a.title, "a.md");
        // Somewhere new from here: forward is gone, as a browser forgets it.
        let c = Pane::preview(&url("src/b.rs"), &dir, 60).unwrap().after(a);
        assert!(!c.can_go(false));
        a = c.go(true);
        assert_eq!(a.title, "a.md");
        assert!(rows(&mut a, 60, 3)[0].contains("← back"));
        // A pane with nowhere to go has no arrows.
        let mut card = Pane::card("/status", "esc close");
        assert!(!rows(&mut card, 30, 2)[0].contains('‹'));
        assert_eq!(card.on_arrow(1, 0), None);
        // Over the margin, the same ‹ goes back, the margin its last step.
        a.over = true;
        assert!(rows(&mut a, 60, 3)[0].starts_with(" ‹ › a.md"));
        assert_eq!(a.head_button(1), None);
        assert_eq!(a.on_arrow(1, 0), Some(true));
        assert_eq!(a.on_arrow(3, 0), Some(false));
        card.over = true;
        assert!(rows(&mut card, 40, 2)[0].starts_with(" ‹ › /status"));
        assert!(rows(&mut card, 40, 2)[0].contains("← back"));
        assert_eq!(card.on_arrow(1, 0), Some(true));
        assert_eq!(card.on_arrow(3, 0), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_wide_name_is_written_again_a_character_at_a_time() {
        let dir = std::env::temp_dir().join(format!("sonny-wide-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("日本.rs"), "x\n").unwrap();
        let mut pane = Pane::preview(&format!("file://{}", dir.display()), &dir, 80).unwrap();
        let area = Rect::new(0, 0, 80, 4);
        let mut buf = Buffer::empty(area);
        pane.render(area, &mut buf, true, "tab", |_| None);
        use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let ev = |kind| MouseEvent {
            kind,
            column: 117,
            row: 4,
            modifiers: KeyModifiers::NONE,
        };
        println!(
            "down {:?}",
            pane.screen
                .mouse(ev(MouseEventKind::Down(MouseButton::Left)))
                .is_some()
        );
        println!(
            "up {:?}",
            pane.screen
                .mouse(ev(MouseEventKind::Up(MouseButton::Left)))
                .map(|a| match a {
                    crate::screen::Action::Open(u) => u,
                    _ => "copy".into(),
                })
        );
        let cells = pane.screen.link_cells(&buf);
        // No cell a wide character goes on into is written over.
        for (x, y, _) in &cells {
            assert_ne!(buf[(*x, *y)].symbol(), " ", "column {x}");
        }
        let text: String = cells.iter().map(|(_, _, c)| c.symbol()).collect();
        assert!(text.contains("日") && text.contains("本"));
        assert!(text.ends_with("s\x1b]8;;\x1b\\"), "{text:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_markdown_table_is_laid_again_at_the_width_it_is_drawn_at() {
        let dir = std::env::temp_dir().join(format!("sonny-table-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.md");
        std::fs::write(
            &file,
            "| Time | Milestone |\n|---|---|\n| 02:29 | The question was accepted by the lead |\n",
        )
        .unwrap();
        let url = format!("file://{}", file.display());
        // Read for a wider pane than it is drawn in, as a pane opened
        // before the window narrowed is.
        let mut pane = Pane::preview(&url, &dir, 60).unwrap();
        for width in [40, 30, 50] {
            let shown = rows(&mut pane, width, 12);
            let table: Vec<&String> = shown.iter().filter(|row| row.contains('│')).collect();
            assert!(!table.is_empty(), "{shown:?}");
            // Every row of the table is whole: it opens and closes on its
            // own row, none wrapped into a piece of another.
            for row in &table {
                assert!(row.trim_start().starts_with('│'), "{width}: {shown:?}");
                assert!(row.ends_with('│'), "{width}: {shown:?}");
            }
            assert!(shown.iter().any(|row| row.trim_start().starts_with('┌')));
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_tall_markdown_file_opens_at_its_top_whatever_width_it_is_drawn_at() {
        let dir = std::env::temp_dir().join(format!("sonny-tall-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.md");
        let body: String = (0..60).map(|i| format!("Line {i}.\n\n")).collect();
        std::fs::write(&file, format!("# Top\n\n{body}")).unwrap();
        let url = format!("file://{}", file.display());
        let mut pane = Pane::preview(&url, &dir, 60).unwrap();
        let shown = rows(&mut pane, 40, 10);
        assert!(shown.iter().any(|row| row.contains("# Top")), "{shown:?}");
        // Scrolled to the end, a narrower width keeps it at the end.
        pane.screen.latest();
        rows(&mut pane, 40, 10);
        let shown = rows(&mut pane, 30, 10);
        assert!(
            shown.iter().any(|row| row.contains("Line 59.")),
            "{shown:?}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_long_code_line_wraps_under_its_code() {
        let dir = std::env::temp_dir().join(format!("sonny-wrap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, "one two three four five six\n").unwrap();
        let mut pane = Pane::preview(&format!("file://{}", file.display()), &dir, 20).unwrap();
        let shown = rows(&mut pane, 20, 4);
        assert_eq!(shown[2], " 1  one two three");
        assert_eq!(shown[3], "    four five six");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_reads_at_the_conversations_width_in_a_wide_pane() {
        let dir = std::env::temp_dir().join(format!("sonny-cap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, format!("{}\n", "word ".repeat(60))).unwrap();
        let mut pane = Pane::preview(&format!("file://{}", file.display()), &dir, 100).unwrap();
        let shown = rows(&mut pane, 150, 5);
        // The title and the file's rows stop at 100 columns.
        assert!(shown[0].ends_with("esc close"), "{shown:?}");
        assert!(shown[2].width() <= crate::measure(), "{}", shown[2].width());
        assert!(shown[3].starts_with("    word"), "{shown:?}");
        // Code goes as wide as the pane.
        let code = dir.join("a.rs");
        std::fs::write(&code, format!("let x = \"{}\";\n", "w".repeat(120))).unwrap();
        let mut pane = Pane::preview(&format!("file://{}", code.display()), &dir, 150).unwrap();
        assert!(rows(&mut pane, 150, 4)[2].width() > crate::measure());
        // Markdown is prose.
        let doc = dir.join("a.md");
        std::fs::write(&doc, format!("{}\n", "word ".repeat(40))).unwrap();
        let mut pane = Pane::preview(&format!("file://{}", doc.display()), &dir, 150).unwrap();
        let shown = rows(&mut pane, 150, 4);
        assert!(shown[2].width() <= crate::measure(), "{shown:?}");
        // A card is prose too, its title over it as wide; one that fills
        // the pane, as the sky does, keeps the pane's width.
        let mut card = Pane::card("status", "esc close");
        card.set_card(vec![Line::from("x".repeat(140))], "esc close");
        let shown = rows(&mut card, 150, 4);
        assert_eq!(shown[2].width(), crate::measure());
        assert!(shown[0].width() <= crate::measure() + 1, "{shown:?}");
        assert!(shown[0].ends_with("esc close"), "{shown:?}");
        card.prose = false;
        assert_eq!(rows(&mut card, 150, 3)[2].width(), 140);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_picture_file_says_what_it_is_where_none_are_drawn() {
        let dir = std::env::temp_dir().join(format!("sonny-png-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("shot.png");
        image::RgbaImage::new(4, 4).save(&file).unwrap();
        let mut pane = Pane::preview(&format!("file://{}", file.display()), &dir, 60).unwrap();
        let shown = rows(&mut pane, 60, 3);
        assert!(shown[2].starts_with(" a picture, "), "{shown:?}");
        assert!(shown[2].contains("draws no pictures"), "{shown:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn markdown_reads_as_a_reply_does() {
        let dir = std::env::temp_dir().join(format!("sonny-md-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("README.md");
        std::fs::write(
            &file,
            "# Title\n\nSome `code`.\n\n```rust\nfn a() {}\n```\n",
        )
        .unwrap();
        let mut pane = Pane::preview(&format!("file://{}", file.display()), &dir, 30).unwrap();
        let shown = rows(&mut pane, 30, 8);
        assert_eq!(shown[2], " # Title");
        assert_eq!(shown[4], " Some code.");
        assert_eq!(shown[6], "   fn a() {}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_same_rows_for_another_note_point_at_it() {
        use crate::screen::{Mark, Tag};
        let mut pane = Pane::margin();
        let lines = vec![Line::from(" ✎ Repro")];
        let head = (Line::from("calc"), Vec::new());
        pane.set_margin(
            lines.clone(),
            vec![(0, Mark::Tag(Tag::Note(1)))],
            head.clone(),
        );
        pane.set_margin(lines, vec![(0, Mark::Tag(Tag::Note(2)))], head);
        assert!(pane.screen.show_tagged(Tag::Note(2), 0));
        assert!(!pane.screen.show_tagged(Tag::Note(1), 0));
    }
}
