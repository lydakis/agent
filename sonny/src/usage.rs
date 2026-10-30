//! `/usage`: the tokens the project's agents spent, or every agent's, in
//! the last day, week or month: in all, over time as stacked bars, by
//! agent (or project), by model, and by what started the turns: the
//! commons, a trigger, the margin, or a prompt. One daemon read, `usage`, each time
//! the view changes and every minute while it is open: it reads every
//! matching turn, so it is not asked for often.
use crate::background::Scope;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    style::{Color, Style, Stylize},
    text::{Line, Span},
};
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

/// How often the open view reads again.
const FRESH: Duration = Duration::from_secs(60);
/// The most rows the card scrolls by: more than it ever has.
const MOST_SCROLL: usize = 200;
/// The chart's height in rows.
const ROWS: usize = 8;
/// The most agents or models listed, the rest summed on one row.
const LISTED: usize = 8;
const HOUR: i64 = 3_600_000;
const DAY: i64 = 24 * HOUR;

/// What a stacked bar is made of, bottom up (or left to right): output,
/// input the provider read fresh, and input it read from its cache.
const OUTPUT: Color = Color::Magenta;
const FRESH_INPUT: Color = Color::Cyan;
const CACHED: Color = Color::Blue;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Period {
    Day,
    Week,
    Month,
}

impl Period {
    fn label(self) -> &'static str {
        match self {
            Period::Day => "last 24 hours",
            Period::Week => "last 7 days",
            Period::Month => "last 30 days",
        }
    }

    /// Where its chart starts, its bars' width, and how many there are,
    /// at `now`: whole local hours for a day, quarter days for a week and
    /// days for a month, the last bar the one now is in.
    fn window(self, now: i64) -> (i64, i64, usize) {
        match self {
            Period::Day => {
                let at = local(now / 1000);
                let hour = now - (at.minute * 60 + at.second) * 1000;
                (hour - 23 * HOUR, HOUR, 24)
            }
            Period::Week => (midnight(now) - 6 * DAY, 6 * HOUR, 28),
            Period::Month => (midnight(now) - 29 * DAY, DAY, 30),
        }
    }
}

/// What some turns spent.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Spent {
    pub turns: u64,
    pub output: u64,
    pub fresh: u64,
    pub cached: u64,
}

impl Spent {
    fn of(row: &Value) -> Self {
        let input = row["input_tokens"].as_u64().unwrap_or(0);
        let cached = row["cached_input_tokens"].as_u64().unwrap_or(0).min(input);
        Self {
            turns: row["turns"].as_u64().unwrap_or(0),
            output: row["output_tokens"].as_u64().unwrap_or(0),
            fresh: input - cached,
            cached,
        }
    }
    fn add(&mut self, other: Spent) {
        self.turns += other.turns;
        self.output += other.output;
        self.fresh += other.fresh;
        self.cached += other.cached;
    }
    /// Its parts, bottom up, the cached reads left out when `cached` is.
    fn parts(&self, cached: bool) -> [(u64, Color); 3] {
        [
            (self.output, OUTPUT),
            (self.fresh, FRESH_INPUT),
            (if cached { self.cached } else { 0 }, CACHED),
        ]
    }
    fn total(&self, cached: bool) -> u64 {
        self.parts(cached).iter().map(|(n, _)| n).sum()
    }
}

/// What a key did to the view.
#[derive(Debug, PartialEq)]
pub enum Key {
    Stay,
    Close,
}

/// `/usage`, open.
pub struct View {
    scope: Scope,
    /// Every agent in the store, not only the project's.
    all: bool,
    period: Period,
    /// The cached reads count in the chart and bars.
    cached: bool,
    /// The last read: the window it asked for and what came back.
    read: Option<Read>,
    error: Option<String>,
    /// When it was read last; none until it is, or once it must be again.
    at: Option<Instant>,
    /// Rows scrolled past on its own screen, inline; the pane scrolls
    /// itself.
    pub scroll: usize,
}

struct Read {
    since: i64,
    bucket: i64,
    buckets: usize,
    all: bool,
    period: Period,
    usage: Value,
}

impl View {
    pub fn new(scope: Scope) -> Self {
        Self {
            scope,
            all: false,
            period: Period::Day,
            cached: true,
            read: None,
            error: None,
            at: None,
            scroll: 0,
        }
    }

    /// Whether it should be read again now.
    pub fn due(&self) -> bool {
        self.at.is_none_or(|at| at.elapsed() >= FRESH)
    }

    /// The `usage` request for what it shows, at `now`, and the window
    /// it asks for.
    pub fn query(&self, now: i64) -> (Value, (i64, i64, usize)) {
        let (since, bucket, buckets) = self.period.window(now);
        let mut query = json!({"since_ms": since, "bucket_ms": bucket, "limit": 256});
        if !self.all {
            query["name"] = json!(self.scope.glob);
        }
        (query, (since, bucket, buckets))
    }

    /// What the read asked with `window` came back with.
    pub fn loaded(&mut self, window: (i64, i64, usize), usage: Result<Value, String>) {
        self.at = Some(Instant::now());
        match usage {
            Ok(usage) => {
                let (since, bucket, buckets) = window;
                self.error = None;
                self.read = Some(Read {
                    since,
                    bucket,
                    buckets,
                    all: self.all,
                    period: self.period,
                    usage,
                });
            }
            Err(error) => self.error = Some(error),
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> Key {
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
        {
            return match key.code {
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Key::Close,
                _ => Key::Stay,
            };
        }
        let period = match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => return Key::Close,
            KeyCode::Char('d') => Period::Day,
            KeyCode::Char('w') => Period::Week,
            KeyCode::Char('m') => Period::Month,
            KeyCode::Left | KeyCode::Char('h') => match self.period {
                Period::Month => Period::Week,
                _ => Period::Day,
            },
            KeyCode::Right | KeyCode::Char('l') => match self.period {
                Period::Day => Period::Week,
                _ => Period::Month,
            },
            KeyCode::Char('a') => {
                self.all = !self.all;
                self.at = None;
                return Key::Stay;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll = self.scroll.saturating_sub(1);
                return Key::Stay;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll = (self.scroll + 1).min(MOST_SCROLL);
                return Key::Stay;
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(10);
                return Key::Stay;
            }
            KeyCode::PageDown | KeyCode::Char(' ') => {
                self.scroll = (self.scroll + 10).min(MOST_SCROLL);
                return Key::Stay;
            }
            KeyCode::Home => {
                self.scroll = 0;
                return Key::Stay;
            }
            KeyCode::Char('c') => {
                self.cached = !self.cached;
                return Key::Stay;
            }
            _ => return Key::Stay,
        };
        if period != self.period {
            self.period = period;
            self.at = None;
        }
        Key::Stay
    }

    /// The card at `width`.
    pub fn card(&self, width: usize) -> Vec<Line<'static>> {
        let whose = match self.all {
            true => "every agent".to_owned(),
            false => format!("{}'s agents", self.scope.project),
        };
        let mut out = vec![Line::from(vec![
            Span::raw("Usage").bold(),
            Span::raw(format!(" · {whose} · {}", self.period.label())).dim(),
        ])];
        if let Some(error) = &self.error {
            out.push(Line::default());
            out.push(Line::from(format!("could not read usage: {error}")).fg(Color::Yellow));
        }
        let Some(read) = &self.read else {
            if self.error.is_none() {
                out.push(Line::from("reading…").dim());
            }
            return out;
        };
        // Until the read for a new choice lands, the last one says what
        // it shows.
        if read.all != self.all || read.period != self.period {
            out[0] = Line::from(vec![
                Span::raw("Usage").bold(),
                Span::raw(" · reading…").dim(),
            ]);
        }
        let usage = &read.usage;
        let total = Spent::of(usage);
        out.push(Line::default());
        if total.turns == 0 {
            out.push(Line::from(format!(
                "No turns in the {}.",
                read.period.label()
            )));
            out.push(
                Line::from(match self.all {
                    true => "a turn counts where it started".to_owned(),
                    false => "a shows every agent's".to_owned(),
                })
                .dim(),
            );
            return out;
        }
        let input = total.fresh + total.cached;
        let mut head = vec![
            Span::raw(tokens(total.output + input)).bold(),
            Span::raw(" tokens · "),
            Span::raw(format!("{} turns", total.turns)),
        ];
        if let Some(share) = (total.cached * 100).checked_div(input) {
            head.push(Span::raw(format!(" · {share}% of input from cache")));
        }
        out.push(Line::from(head));
        out.push(Line::default());
        out.extend(self.chart(read, width));
        out.push(legend(&total, self.cached));
        out.push(Line::default());
        // The project's agents by name; every agent's by project, as a
        // fleet's names start with theirs.
        let mut rows: Vec<(String, Spent)> = Vec::new();
        for bot in usage["bots"].as_array().into_iter().flatten() {
            let name = bot["bot"].as_str().unwrap_or("?");
            let name = match self.all {
                true => name.split('.').next().unwrap_or(name).to_owned(),
                false => self.scope.short(name),
            };
            match rows.iter_mut().find(|(n, _)| *n == name) {
                Some((_, spent)) => spent.add(Spent::of(bot)),
                None => rows.push((name, Spent::of(bot))),
            }
        }
        rows.sort_by(|a, b| {
            b.1.total(self.cached)
                .cmp(&a.1.total(self.cached))
                .then_with(|| a.0.cmp(&b.0))
        });
        let (heading, noun) = match self.all {
            true => ("By project", "projects"),
            false => ("By agent", "agents"),
        };
        // Past the first few, and the bots the daemon summed, on one row.
        let hidden = rows.len().saturating_sub(LISTED);
        let mut rest = Spent::default();
        for (_, spent) in rows.drain(rows.len() - hidden..) {
            rest.add(spent);
        }
        let summed = usage["other_bots"].is_object();
        if summed {
            rest.add(Spent::of(&usage["other_bots"]));
        }
        match (hidden, summed) {
            (0, false) => {}
            (n, false) => rows.push((format!("{n} more {noun}"), rest)),
            (_, true) => rows.push(("the rest".to_owned(), rest)),
        }
        out.extend(self.bars(heading, &rows, &total, width));
        let mut models: Vec<(String, Spent)> = usage["models"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|m| (m["model"].as_str().unwrap_or("?").to_owned(), Spent::of(m)))
            .collect();
        if models.len() > LISTED {
            let mut more = Spent::default();
            let n = models.len() - LISTED;
            for (_, spent) in models.drain(LISTED..) {
                more.add(spent);
            }
            models.push((format!("{n} more models"), more));
        }
        out.push(Line::default());
        out.extend(self.bars("By model", &models, &total, width));
        // What Sonny's own features started, by the origin it gave their
        // turns; one with none was a prompt, a person's or an agent's.
        let origins: Vec<(String, Spent)> = usage["origins"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|o| {
                (
                    o["origin"].as_str().unwrap_or("prompts").to_owned(),
                    Spent::of(o),
                )
            })
            .collect();
        if origins.iter().any(|(origin, _)| origin != "prompts") {
            out.push(Line::default());
            out.extend(self.bars("By what started it", &origins, &total, width));
        }
        out
    }

    /// The stacked bars over time, with the most a bar holds at the top
    /// of the axis and the times under it.
    fn chart(&self, read: &Read, width: usize) -> Vec<Line<'static>> {
        let mut buckets = vec![Spent::default(); read.buckets];
        for row in read.usage["series"].as_array().into_iter().flatten() {
            let at = row["at_ms"].as_i64().unwrap_or(0);
            let index = (at - read.since) / read.bucket.max(1);
            if let Some(bucket) = usize::try_from(index)
                .ok()
                .and_then(|index| buckets.get_mut(index))
            {
                bucket.add(Spent::of(row));
            }
        }
        let most = buckets
            .iter()
            .map(|b| b.total(self.cached))
            .max()
            .unwrap_or(0)
            .max(1);
        let top = tokens(most);
        let axis = top.width().max(1);
        // Each bar as wide as fits, up to four columns with a gap.
        let room = width.saturating_sub(axis + 2);
        let step = (room / read.buckets.max(1)).clamp(1, 4);
        let bar = if step >= 3 { step - 1 } else { step };
        let eighths = (ROWS * 8) as u64;
        let heights: Vec<[u64; 3]> = buckets
            .iter()
            .map(|b| {
                let mut sum = 0;
                let mut tops = [0; 3];
                for (i, (n, _)) in b.parts(self.cached).iter().enumerate() {
                    sum += n;
                    tops[i] = (sum * eighths).div_ceil(most);
                }
                tops
            })
            .collect();
        let mut out = Vec::new();
        for row in (0..ROWS).rev() {
            let label = match row {
                r if r == ROWS - 1 => format!("{top:>axis$} ┤"),
                0 => format!("{:>axis$} ┼", 0),
                _ => format!("{:>axis$} │", ""),
            };
            let mut spans = vec![Span::raw(label).dim()];
            for (b, tops) in buckets.iter().zip(&heights) {
                let cell = cell(tops, &b.parts(self.cached), row);
                spans.push(Span::styled(cell.0.to_string().repeat(bar), cell.1));
                if step > bar {
                    spans.push(Span::raw(" "));
                }
            }
            out.push(Line::from(spans));
        }
        // The times under the bars, each where its bar starts, those that
        // would run into the one before left out.
        let mut times = " ".repeat(axis + 2);
        let mut at = 0;
        for index in 0..read.buckets {
            let start = read.since + index as i64 * read.bucket;
            let Some(text) = time_label(read.period, start, index) else {
                continue;
            };
            let column = index * step;
            if column < at {
                continue;
            }
            times.push_str(&" ".repeat(column - at));
            times.push_str(&text);
            at = column + text.width() + 1;
            times.push(' ');
        }
        out.push(Line::from(times.trim_end().to_owned()).dim());
        out
    }

    /// `rows` as stacked bars under `heading`, each its share of `total`.
    fn bars(
        &self,
        heading: &str,
        rows: &[(String, Spent)],
        total: &Spent,
        width: usize,
    ) -> Vec<Line<'static>> {
        let mut out = vec![Line::from(heading.to_owned()).bold()];
        let name_width = rows
            .iter()
            .map(|(name, _)| name.width())
            .max()
            .unwrap_or(0)
            .clamp(4, 28);
        let all = total.total(self.cached).max(1);
        let most = rows
            .iter()
            .map(|(_, s)| s.total(self.cached))
            .max()
            .unwrap_or(0)
            .max(1);
        // The name, the bar, then the tokens and share.
        let figures = 13;
        let room = width.saturating_sub(name_width + 2 + figures).clamp(4, 60);
        for (name, spent) in rows {
            let sum = spent.total(self.cached);
            let mut spans = vec![Span::raw(format!(
                "{:<name_width$}  ",
                crate::transcript::cut(name, name_width)
            ))];
            spans.extend(hbar(&spent.parts(self.cached), most, room));
            spans.push(Span::raw(format!(" {:>7}", tokens(sum))));
            spans.push(Span::raw(format!(" {:>3}%", sum * 100 / all)).dim());
            out.push(Line::from(spans));
        }
        out
    }

    /// The line under the card: the keys it takes.
    pub fn footer(&self) -> Line<'static> {
        let scope = match self.all {
            true => "a this project",
            false => "a every agent",
        };
        let cached = match self.cached {
            true => "c hide cached",
            false => "c show cached",
        };
        Line::from(crate::hint::spans(&format!(
            "  d/w/m period · {scope} · {cached} · esc close"
        )))
    }
}

/// One cell of a stacked bar, `row` rows up: the block that fills it to
/// where its part ends, in that part's color, over the color of the part
/// above it. `tops` are where each part ends, in eighths of a row.
fn cell(tops: &[u64; 3], parts: &[(u64, Color); 3], row: usize) -> (char, Style) {
    const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let low = row as u64 * 8;
    let Some(part) = (0..3).find(|&i| tops[i] > low && parts[i].0 > 0) else {
        return (' ', Style::new());
    };
    let end = tops[part];
    let style = Style::new().fg(parts[part].1);
    if end >= low + 8 {
        return ('█', style);
    }
    let above = (part + 1..3).find(|&i| tops[i] > end && parts[i].0 > 0);
    let style = match above {
        Some(i) => style.bg(parts[i].1),
        None => style,
    };
    (BLOCKS[(end - low - 1) as usize], style)
}

/// A bar `room` columns wide at its longest, `most`, of `parts` left to
/// right, each part's end in eighths of a column.
fn hbar(parts: &[(u64, Color); 3], most: u64, room: usize) -> Vec<Span<'static>> {
    const BLOCKS: [char; 8] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];
    let eighths = room as u64 * 8;
    let mut sum = 0;
    let mut ends = [0u64; 3];
    for (i, (n, _)) in parts.iter().enumerate() {
        sum += n;
        ends[i] = (sum * eighths).div_ceil(most);
    }
    let mut spans = Vec::new();
    for column in 0..room {
        let low = column as u64 * 8;
        let Some(part) = (0..3).find(|&i| ends[i] > low && parts[i].0 > 0) else {
            spans.push(Span::raw(" "));
            continue;
        };
        let end = ends[part];
        let style = Style::new().fg(parts[part].1);
        if end >= low + 8 {
            spans.push(Span::styled("█", style));
            continue;
        }
        let style = match (part + 1..3).find(|&i| ends[i] > end && parts[i].0 > 0) {
            Some(i) => style.bg(parts[i].1),
            None => style,
        };
        spans.push(Span::styled(
            BLOCKS[(end - low - 1) as usize].to_string(),
            style,
        ));
    }
    spans
}

/// What each color is, with how much of it there was.
fn legend(total: &Spent, cached: bool) -> Line<'static> {
    let mut spans = vec![
        Span::raw("█").fg(OUTPUT),
        Span::raw(format!(" output {}   ", tokens(total.output))),
        Span::raw("█").fg(FRESH_INPUT),
        Span::raw(format!(" input {}   ", tokens(total.fresh))),
    ];
    match cached {
        true => {
            spans.push(Span::raw("█").fg(CACHED));
            spans.push(Span::raw(format!(" cached {}", tokens(total.cached))));
        }
        false => spans.push(Span::raw(format!("cached {} hidden", tokens(total.cached))).dim()),
    }
    Line::from(spans)
}

/// A count of tokens in a few characters: 950, 12.3k, 4.5M.
pub fn tokens(n: u64) -> String {
    let f = n as f64;
    match n {
        0..1_000 => n.to_string(),
        1_000..10_000 => format!("{:.1}k", f / 1e3),
        10_000..1_000_000 => format!("{:.0}k", f / 1e3),
        1_000_000..10_000_000 => format!("{:.1}M", f / 1e6),
        10_000_000..1_000_000_000 => format!("{:.0}M", f / 1e6),
        _ => format!("{:.1}B", f / 1e9),
    }
}

/// The time under bar `index`, starting at `start`, when one goes there:
/// every sixth hour of a day, each day's name in a week, every fifth day
/// of a month.
fn time_label(period: Period, start: i64, index: usize) -> Option<String> {
    let at = local(start / 1000);
    match period {
        Period::Day => (at.hour % 6 == 0).then(|| format!("{:02}", at.hour)),
        Period::Week => (at.hour < 6).then(|| {
            ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"][at.weekday as usize % 7].to_owned()
        }),
        Period::Month => index
            .is_multiple_of(5)
            .then(|| format!("{} {}", MONTHS[at.month as usize % 12], at.day)),
    }
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// A local wall-clock time, with its weekday (0 is Sunday) and month (0
/// is January).
struct Local {
    month: i64,
    day: i64,
    weekday: i64,
    hour: i64,
    minute: i64,
    second: i64,
}

fn local(epoch: i64) -> Local {
    // SAFETY: localtime_r fills the zeroed struct it is given and nothing else.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&(epoch as libc::time_t), &mut tm);
        tm
    };
    Local {
        month: tm.tm_mon as i64,
        day: tm.tm_mday as i64,
        weekday: tm.tm_wday as i64,
        hour: tm.tm_hour as i64,
        minute: tm.tm_min as i64,
        second: tm.tm_sec as i64,
    }
}

/// The local midnight that starts the day `now` (in milliseconds) is in.
fn midnight(now: i64) -> i64 {
    // SAFETY: localtime_r fills the zeroed struct, and mktime reads and
    // normalizes it.
    let epoch = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&((now / 1000) as libc::time_t), &mut tm);
        tm.tm_hour = 0;
        tm.tm_min = 0;
        tm.tm_sec = 0;
        tm.tm_isdst = -1;
        libc::mktime(&mut tm)
    };
    if epoch == -1 {
        return now - now.rem_euclid(DAY);
    }
    epoch as i64 * 1000
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> Scope {
        Scope {
            project: "p".into(),
            lead: "p.lead".into(),
            glob: "p.*".into(),
        }
    }

    fn text(lines: &[Line]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn tokens_read_in_a_few_characters() {
        assert_eq!(tokens(950), "950");
        assert_eq!(tokens(12_345), "12k");
        assert_eq!(tokens(4_560_000), "4.6M");
        assert_eq!(tokens(2_100_000_000), "2.1B");
    }

    #[test]
    fn a_cell_fills_to_its_part_over_the_part_above() {
        let parts = [(10, OUTPUT), (10, FRESH_INPUT), (10, CACHED)];
        // Output ends three eighths up the first row, input at a row and
        // a half, cached at three rows.
        let tops = [3, 12, 24];
        assert_eq!(
            cell(&tops, &parts, 0),
            ('▃', Style::new().fg(OUTPUT).bg(FRESH_INPUT))
        );
        assert_eq!(
            cell(&tops, &parts, 1),
            ('▄', Style::new().fg(FRESH_INPUT).bg(CACHED))
        );
        assert_eq!(cell(&tops, &parts, 2), ('█', Style::new().fg(CACHED)));
        assert_eq!(cell(&tops, &parts, 3).0, ' ');
    }

    #[test]
    fn the_view_asks_for_its_scope_and_period_and_draws_what_came_back() {
        let mut view = View::new(scope());
        assert!(view.due());
        let now = 1_800_000_000_000;
        let (query, window) = view.query(now);
        assert_eq!(query["name"], "p.*");
        assert_eq!(query["bucket_ms"], HOUR);
        assert_eq!(window.2, 24);
        let (since, bucket, _) = window;
        view.loaded(
            window,
            Ok(json!({
                "turns": 3, "input_tokens": 3000, "cached_input_tokens": 2000, "output_tokens": 300,
                "bots": [
                    {"bot": "p.lead", "turns": 2, "input_tokens": 2000, "cached_input_tokens": 1500, "output_tokens": 200},
                    {"bot": "p.fix", "turns": 1, "input_tokens": 1000, "cached_input_tokens": 500, "output_tokens": 100},
                ],
                "other_bots": null,
                "models": [{"model": "openai/gpt-6-sol", "turns": 3, "input_tokens": 3000, "cached_input_tokens": 2000, "output_tokens": 300}],
                "origins": [
                    {"origin": null, "turns": 2, "input_tokens": 2000, "cached_input_tokens": 1500, "output_tokens": 200},
                    {"origin": "commons", "turns": 1, "input_tokens": 1000, "cached_input_tokens": 500, "output_tokens": 100},
                ],
                "series": [
                    {"at_ms": since, "turns": 1, "input_tokens": 1000, "cached_input_tokens": 500, "output_tokens": 100},
                    {"at_ms": since + 23 * bucket, "turns": 2, "input_tokens": 2000, "cached_input_tokens": 1500, "output_tokens": 200},
                ],
            })),
        );
        assert!(!view.due());
        let card = text(&view.card(60));
        assert!(
            card.contains("3.3k tokens · 3 turns · 66% of input from cache"),
            "{card}"
        );
        assert!(card.contains("By agent"), "{card}");
        assert!(card.contains("lead "), "{card}");
        assert!(card.contains("fix "), "{card}");
        assert!(card.contains("openai/gpt-6-sol"), "{card}");
        // What the commons started, beside the prompts.
        assert!(card.contains("By what started it"), "{card}");
        assert!(card.contains("commons "), "{card}");
        assert!(card.contains("prompts "), "{card}");
        // The busiest bar reaches the top row.
        let top = view.card(60)[4].clone();
        assert!(text(&[top]).ends_with('█'), "{card}");

        // Inline, the card scrolls on its own keys.
        view.key(KeyEvent::from(KeyCode::Down));
        view.key(KeyEvent::from(KeyCode::PageDown));
        assert_eq!(view.scroll, 11);
        view.key(KeyEvent::from(KeyCode::Home));
        assert_eq!(view.scroll, 0);

        // Every agent's, by project, asks with no name.
        view.key(KeyEvent::from(KeyCode::Char('a')));
        assert!(view.due());
        assert!(view.query(now).0.get("name").is_none());
        view.key(KeyEvent::from(KeyCode::Char('m')));
        let (query, window) = view.query(now);
        assert_eq!((query["bucket_ms"].as_i64(), window.2), (Some(DAY), 30));
        assert_eq!(view.key(KeyEvent::from(KeyCode::Esc)), Key::Close);
    }

    #[test]
    fn a_week_starts_at_a_midnight_six_days_back() {
        let now = 1_800_000_000_000;
        let (since, bucket, buckets) = Period::Week.window(now);
        assert_eq!((bucket, buckets), (6 * HOUR, 28));
        assert_eq!(local(since / 1000).hour, 0);
        assert!(now - since <= 7 * DAY && now - since > 6 * DAY - 2 * HOUR);
    }
}
