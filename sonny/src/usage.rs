//! `/usage`: the tokens the project's agents spent, or every agent's, in
//! the last day, week or month: in all, over time as stacked bars, by
//! agent (or project), by model, and by what started the turns: the
//! commons, a trigger, the margin, or a prompt. One daemon read, `usage`, each time
//! the view changes and every minute while it is open: it reads every
//! matching turn, so it is not asked for often. With the person's price
//! sheet (`~/.agent/prices.toml`, or the file config's `prices` names),
//! each model's cost and their sum, priced here from the sheet as it
//! stands at each read; the daemon counts tokens only.
use crate::background::Scope;
use agent_client::config::Prices;
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
    /// What the priced models in it cost.
    pub cost: f64,
    /// How many of the models in it the price sheet names no price for.
    pub unpriced: u64,
    /// Its timed calls, their output, and their time from send to answer
    /// and to the start of output; and the daemon's own time before calls.
    pub timed: u64,
    pub timed_output: u64,
    pub model_ms: u64,
    pub first_ms: u64,
    pub daemon_ms: u64,
}

impl Spent {
    fn of(row: &Value) -> Self {
        let input = row["input_tokens"].as_u64().unwrap_or(0);
        let cached = row["cached_input_tokens"].as_u64().unwrap_or(0).min(input);
        let count = |key: &str| row[key].as_u64().unwrap_or(0);
        Self {
            turns: count("turns"),
            output: count("output_tokens"),
            fresh: input - cached,
            cached,
            timed: count("timed_rounds"),
            timed_output: count("timed_output_tokens"),
            model_ms: count("model_ms"),
            first_ms: count("first_token_ms"),
            daemon_ms: count("daemon_ms"),
            ..Self::default()
        }
    }
    fn add(&mut self, other: Spent) {
        self.turns += other.turns;
        self.output += other.output;
        self.fresh += other.fresh;
        self.cached += other.cached;
        self.cost += other.cost;
        self.unpriced += other.unpriced;
        self.timed += other.timed;
        self.timed_output += other.timed_output;
        self.model_ms += other.model_ms;
        self.first_ms += other.first_ms;
        self.daemon_ms += other.daemon_ms;
    }
    /// A model's `row`, priced from `prices` by its `PROVIDER/MODEL` at
    /// the speed that served it.
    fn priced(row: &Value, prices: &[(String, Prices)]) -> Self {
        let mut spent = Self::of(row);
        let id = row["model"].as_str().unwrap_or("");
        let speed = row["speed"].as_str().unwrap_or("standard");
        match agent_client::config::price(prices, id, speed) {
            Some(price) => {
                let count = |key: &str| row[key].as_u64().unwrap_or(0);
                spent.cost = price.cost(
                    count("input_tokens"),
                    count("cached_input_tokens"),
                    count("cache_write_tokens"),
                    count("cache_write_1h_tokens"),
                    count("output_tokens"),
                );
            }
            None => spent.unpriced = 1,
        }
        spent
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
    /// The price sheet as it was at the last read; none, no costs.
    prices: Vec<(String, Prices)>,
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
            prices: Vec::new(),
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

    /// What the read asked with `window` came back with, priced at
    /// `prices`.
    pub fn loaded(
        &mut self,
        window: (i64, i64, usize),
        usage: Result<Value, String>,
        prices: Vec<(String, Prices)>,
    ) {
        self.at = Some(Instant::now());
        self.prices = prices;
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
        // The pane's title names it: its first row says whose and when.
        let mut out = vec![Line::from(vec![
            Span::raw(whose.clone()).bold(),
            Span::raw(format!(" · {}", self.period.label())).dim(),
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
                Span::raw(whose).bold(),
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
        // Each model priced by its own id and speed, before ids of one
        // model share a row.
        let mut models: Vec<Row> = Vec::new();
        let mut spent = Spent::default();
        for row in usage["models"].as_array().into_iter().flatten() {
            let id = row["model"].as_str().unwrap_or("?");
            let provider = crate::models::provider(id).unwrap_or_default();
            // A faster tier's spend is a row of its own, priced at its rates.
            let name = match row["speed"].as_str() {
                Some(speed) if speed != "standard" => {
                    format!("{} · {speed}", crate::models::label(id))
                }
                _ => crate::models::label(id),
            };
            let priced = Spent::priced(row, &self.prices);
            spent.add(priced);
            add(&mut models, name, provider.to_owned(), priced);
        }
        let costs = !self.prices.is_empty();
        if costs {
            head.push(Span::raw(" · "));
            head.push(Span::raw(money(spent.cost)).bold());
            if spent.unpriced > 0 {
                let noun = if spent.unpriced == 1 {
                    "model"
                } else {
                    "models"
                };
                head.push(Span::raw(format!(" · {} {noun} not priced", spent.unpriced)).dim());
            }
        }
        out.push(Line::from(head));
        out.push(Line::default());
        out.extend(self.chart(read, width));
        out.push(legend(&total, self.cached));
        out.push(Line::default());
        // The project's agents by name; every agent's by project, as a
        // fleet's names start with theirs.
        let mut rows: Vec<Row> = Vec::new();
        for bot in usage["bots"].as_array().into_iter().flatten() {
            let name = bot["bot"].as_str().unwrap_or("?");
            let name = match self.all {
                true => name.split('.').next().unwrap_or(name).to_owned(),
                false => self.scope.short(name),
            };
            add(&mut rows, name, String::new(), Spent::of(bot));
        }
        self.sort(&mut rows);
        let (heading, noun) = match self.all {
            true => ("By project", "projects"),
            false => ("By agent", "agents"),
        };
        // Past the first few, and the bots the daemon summed, on one row.
        let hidden = rows.len().saturating_sub(LISTED);
        let mut rest = Spent::default();
        for (_, _, spent) in rows.drain(rows.len() - hidden..) {
            rest.add(spent);
        }
        let summed = usage["other_bots"].is_object();
        if summed {
            rest.add(Spent::of(&usage["other_bots"]));
        }
        match (hidden, summed) {
            (0, false) => {}
            (n, false) => rows.push((format!("{n} more {noun}"), String::new(), rest)),
            (_, true) => rows.push(("the rest".to_owned(), String::new(), rest)),
        }
        out.extend(self.bars(heading, &rows, &total, width, false));
        // Each model by its name, its provider beside it: ids that name
        // the same model from one provider, as two Bedrock regions' or a
        // longer window's, are one row.
        self.sort(&mut models);
        let speed = speeds(&models[..models.len().min(LISTED)], &total, width);
        if models.len() > LISTED {
            let mut more = Spent::default();
            let n = models.len() - LISTED;
            for (_, _, spent) in models.drain(LISTED..) {
                more.add(spent);
            }
            models.push((format!("{n} more models"), String::new(), more));
        }
        out.push(Line::default());
        out.extend(self.bars("By model", &models, &total, width, costs));
        out.extend(speed);
        out.extend(self.time(usage, width));
        // What Sonny's own features started, by the origin it gave their
        // turns; one with none was a prompt, a person's or an agent's.
        // An automation's turns from before 0.8.0 say `trigger`: one row.
        let mut origins: Vec<Row> = Vec::new();
        for o in usage["origins"].as_array().into_iter().flatten() {
            let origin = match o["origin"].as_str() {
                Some(origin) if crate::trigger::sent_it(origin) => crate::trigger::ORIGIN,
                origin => origin.unwrap_or("prompts"),
            };
            match origins.iter_mut().find(|(name, _, _)| name == origin) {
                Some((_, _, spent)) => spent.add(Spent::of(o)),
                None => origins.push((origin.to_owned(), String::new(), Spent::of(o))),
            }
        }
        if origins.iter().any(|(origin, _, _)| origin != "prompts") {
            out.push(Line::default());
            out.extend(self.bars("By what started it", &origins, &total, width, false));
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

    /// Most first, then by name.
    fn sort(&self, rows: &mut [Row]) {
        rows.sort_by(|a, b| {
            (b.2.total(self.cached).cmp(&a.2.total(self.cached)))
                .then_with(|| (&a.0, &a.1).cmp(&(&b.0, &b.1)))
        });
    }

    /// `rows` as stacked bars under `heading`, each its share of `total`;
    /// a row's detail, a model's provider, muted in a column of its own;
    /// with `costs`, what each cost after its share, or that it is not
    /// priced.
    fn bars(
        &self,
        heading: &str,
        rows: &[Row],
        total: &Spent,
        width: usize,
        costs: bool,
    ) -> Vec<Line<'static>> {
        let mut out = vec![Line::from(heading.to_owned()).bold()];
        let mut name_width = rows
            .iter()
            .map(|(name, _, _)| name.width())
            .max()
            .unwrap_or(0)
            .clamp(4, 28);
        let mut detail_width = (rows.iter())
            .map(|(_, detail, _)| detail.width())
            .max()
            .unwrap_or(0)
            .min(16);
        let all = total.total(self.cached).max(1);
        let most = rows
            .iter()
            .map(|(_, _, s)| s.total(self.cached))
            .max()
            .unwrap_or(0)
            .max(1);
        // The name, its detail, the bar, then the tokens and share, in
        // `width`: a narrow card cuts the detail and then the name, so the
        // bar keeps its least and no row wraps.
        let figures = if costs { 13 + 1 + COST } else { 13 };
        let least = 4;
        let words = width.saturating_sub(figures + least);
        let over = (name_width + 2 + detail_width + 2).saturating_sub(words);
        // A detail that fits is shown whole, however short; one that must
        // be cut goes once fewer than four columns of it would be left.
        detail_width = match (over, detail_width.saturating_sub(over)) {
            (0, n) => n,
            (_, n) if n < 4 => 0,
            (_, n) => n,
        };
        let detail_room = match detail_width {
            0 => 0,
            n => n + 2,
        };
        name_width = name_width.min(words.saturating_sub(2 + detail_room)).max(4);
        let room = (width.saturating_sub(name_width + 2 + detail_room + figures)).clamp(least, 60);
        for (name, detail, spent) in rows {
            let sum = spent.total(self.cached);
            let mut spans = vec![Span::raw(format!(
                "{:<name_width$}  ",
                crate::transcript::cut(name, name_width)
            ))];
            if detail_width > 0 {
                spans.push(
                    Span::raw(format!(
                        "{:<detail_width$}  ",
                        crate::transcript::cut(detail, detail_width)
                    ))
                    .dim(),
                );
            }
            spans.extend(hbar(&spent.parts(self.cached), most, room));
            spans.push(Span::raw(format!(" {:>7}", tokens(sum))));
            spans.push(Span::raw(format!(" {:>3}%", sum * 100 / all)).dim());
            if costs {
                spans.push(match spent.unpriced {
                    0 => Span::raw(format!(" {:>COST$}", money(spent.cost))),
                    _ => Span::raw(format!(" {:>COST$}", "no price")).dim(),
                });
            }
            out.push(Line::from(spans));
        }
        out
    }

    /// Where the finished turns' time went, by agent (or project): each
    /// one's bar as long as its average turn, in the model's calls, its
    /// tools and the rest, with its rounds and tool calls a turn and its
    /// model's output a second. Nothing when no finished turn was timed.
    fn time(&self, usage: &Value, width: usize) -> Vec<Line<'static>> {
        let total = Took::of(&usage["time"]);
        if total.turns == 0 {
            return Vec::new();
        }
        let mut rows: Vec<(String, Took)> = Vec::new();
        for bot in usage["bots"].as_array().into_iter().flatten() {
            let name = bot["bot"].as_str().unwrap_or("?");
            let name = match self.all {
                true => name.split('.').next().unwrap_or(name).to_owned(),
                false => self.scope.short(name),
            };
            let took = Took::of(&bot["time"]);
            if took.turns == 0 {
                continue;
            }
            match rows.iter_mut().find(|(n, _)| *n == name) {
                Some((_, sum)) => sum.add(took),
                None => rows.push((name, took)),
            }
        }
        rows.sort_by(|a, b| b.1.turn_ms.cmp(&a.1.turn_ms).then_with(|| a.0.cmp(&b.0)));
        let hidden = rows.len().saturating_sub(LISTED);
        let mut rest = Took::of(&usage["other_bots"]["time"]);
        for (_, took) in rows.drain(rows.len() - hidden..) {
            rest.add(took);
        }
        if rest.turns > 0 {
            rows.push(("the rest".to_owned(), rest));
        }
        let figures = TIME_FIGURES.len();
        let name_width = (rows.iter().map(|(name, _)| name.width()))
            .max()
            .unwrap_or(0)
            .clamp(4, 28)
            .min(width.saturating_sub(figures + 6).max(4));
        let room = width.saturating_sub(name_width + 2 + figures).clamp(4, 40);
        let most = (rows.iter().map(|(_, took)| took.each()))
            .max()
            .unwrap_or(0)
            .max(1);
        let mut out = vec![
            Line::default(),
            Line::from(vec![
                Span::raw(format!("{:<w$}", "Time", w = name_width + 2 + room)).bold(),
                Span::raw(TIME_FIGURES).dim(),
            ]),
        ];
        for (name, took) in &rows {
            let turns = took.turns.max(1) as f64;
            let mut spans = vec![Span::raw(format!(
                "{:<name_width$}  ",
                crate::transcript::cut(name, name_width)
            ))];
            // Each part of its average turn: the parts summed over its
            // turns, scaled as the turn is to the longest.
            let parts = took
                .parts()
                .map(|(n, color)| (n / took.turns.max(1), color));
            spans.extend(hbar(&parts, most, room));
            spans.push(Span::raw(format!(" {:>7}", span(took.each()))));
            spans.push(Span::raw(format!(" {:>6.1}", took.rounds as f64 / turns)));
            spans.push(Span::raw(format!(" {:>6.1}", took.calls as f64 / turns)));
            spans.push(match took.per_second() {
                Some(rate) => Span::raw(format!(" {rate:>6}")),
                None => Span::raw(format!(" {:>6}", "-")).dim(),
            });
            out.push(Line::from(spans));
        }
        let [(model, _), (tools, _), (rest, _)] = total.parts();
        let all = total.turn_ms.max(1);
        out.push(Line::from(vec![
            Span::raw("█").fg(MODEL_TIME),
            Span::raw(format!(" model {}%   ", model * 100 / all)),
            Span::raw("█").fg(TOOL_TIME),
            Span::raw(format!(" tools {}%   ", tools * 100 / all)),
            Span::raw("█").fg(REST_TIME),
            Span::raw(format!(" waiting and the rest {}%", rest * 100 / all)),
        ]));
        out
    }

    /// The line under the card: the keys it takes.
    pub fn keys(&self) -> String {
        let scope = match self.all {
            true => "a this project",
            false => "a every agent",
        };
        let cached = match self.cached {
            true => "c hide cached",
            false => "c show cached",
        };
        format!("d/w/m period · {scope} · {cached} · esc close")
    }
}

/// The colors of a turn's time: its model's calls, its tools, and the
/// rest.
const MODEL_TIME: Color = Color::Green;
const TOOL_TIME: Color = Color::Yellow;
const REST_TIME: Color = Color::DarkGray;
/// The time section's columns after the bars.
const TIME_FIGURES: &str = "  a turn rounds  calls  tok/s";

/// What some finished turns with timed calls took, from a `usage` row's
/// `time`: their time from start to end, the tool calls that ran to their
/// end and those calls' time, and their own timed model calls: how many,
/// their output and their time.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Took {
    pub turns: u64,
    pub turn_ms: u64,
    pub rounds: u64,
    pub calls: u64,
    pub tool_ms: u64,
    pub model_ms: u64,
    pub output: u64,
}

impl Took {
    fn of(row: &Value) -> Self {
        let count = |key: &str| row[key].as_u64().unwrap_or(0);
        Self {
            turns: count("turns"),
            turn_ms: count("turn_ms"),
            rounds: count("timed_rounds"),
            calls: count("tool_calls"),
            tool_ms: count("tool_ms"),
            model_ms: count("model_ms"),
            output: count("timed_output_tokens"),
        }
    }
    fn add(&mut self, other: Took) {
        self.turns += other.turns;
        self.turn_ms += other.turn_ms;
        self.rounds += other.rounds;
        self.calls += other.calls;
        self.tool_ms += other.tool_ms;
        self.model_ms += other.model_ms;
        self.output += other.output;
    }
    /// Its average turn's time.
    fn each(&self) -> u64 {
        self.turn_ms / self.turns.max(1)
    }
    /// Its model's output tokens a second, from send to answer, as the
    /// speed section counts them.
    fn per_second(&self) -> Option<u64> {
        (self.output * 1000).checked_div(self.model_ms)
    }
    /// Its time in the model's calls, in tools, and the rest: pacing,
    /// summaries, the daemon's own time, gates waited on and failed
    /// attempts. Each within what is left of the turns' time.
    fn parts(&self) -> [(u64, Color); 3] {
        let model = self.model_ms.min(self.turn_ms);
        let tools = self.tool_ms.min(self.turn_ms - model);
        [
            (model, MODEL_TIME),
            (tools, TOOL_TIME),
            (self.turn_ms - model - tools, REST_TIME),
        ]
    }
}

/// A longer time in a few characters: as `took` under a minute, then
/// minutes and seconds, then hours and minutes.
fn span(ms: u64) -> String {
    match ms / 1000 {
        s if s < 60 => took(ms),
        s if s < 3600 => format!("{}m {:02}s", s / 60, s % 60),
        s => format!("{}h {:02}m", s / 3600, s / 60 % 60),
    }
}

/// A row of bars: its name, a detail muted beside it, and what it spent.
type Row = (String, String, Spent);

/// `spent` onto the row with `name` and `detail`, or a new row.
/// How fast each model answered its timed calls: its output tokens a
/// second from send to answer, and its time to first output, on average;
/// and the daemon's own time before each call. Nothing when no call in the
/// window was timed.
fn speeds(models: &[Row], total: &Spent, width: usize) -> Vec<Line<'static>> {
    let timed: Vec<&Row> = (models.iter())
        .filter(|(_, _, spent)| spent.timed > 0 && spent.model_ms > 0)
        .collect();
    if timed.is_empty() {
        return Vec::new();
    }
    let figures = 30;
    let name_width = (timed.iter().map(|(name, _, _)| name.width()))
        .max()
        .unwrap_or(0)
        .clamp(6, 28)
        .min(width.saturating_sub(figures + 2).max(6));
    let mut out = vec![Line::default(), Line::from("Speed").bold()];
    for (name, _, spent) in timed {
        let per_second = spent.timed_output * 1000 / spent.model_ms;
        out.push(Line::from(vec![
            Span::raw(format!(
                "{:<name_width$}  ",
                crate::transcript::cut(name, name_width)
            )),
            Span::raw(format!("{per_second:>5} tok/s")),
            Span::raw(format!(
                "  {:>6} to first",
                took(spent.first_ms / spent.timed)
            ))
            .dim(),
        ]));
    }
    if let Some(each) = total.daemon_ms.checked_div(total.timed) {
        out.push(Line::from(vec![
            Span::raw(format!("{:<name_width$}  ", "daemon")),
            Span::raw(format!("{:>11}", took(each))),
            Span::raw("  before each call").dim(),
        ]));
    }
    out
}

/// A time in milliseconds, in a few characters.
fn took(ms: u64) -> String {
    match ms {
        0 => "<1 ms".to_owned(),
        ms if ms < 1000 => format!("{ms} ms"),
        ms if ms < 10_000 => format!("{:.1} s", ms as f64 / 1000.0),
        ms => format!("{} s", ms / 1000),
    }
}

fn add(rows: &mut Vec<Row>, name: String, detail: String, spent: Spent) {
    match (rows.iter_mut()).find(|(n, d, _)| *n == name && *d == detail) {
        Some((_, _, sum)) => sum.add(spent),
        None => rows.push((name, detail, spent)),
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

/// The columns a cost takes on a model's row.
const COST: usize = 8;

/// A cost in a few characters: $0.42, $123, $12.3k, and under a cent
/// that is not nothing, <$0.01.
fn money(cost: f64) -> String {
    match cost {
        c if c <= 0.0 => "$0".to_owned(),
        c if c < 0.01 => "<$0.01".to_owned(),
        c if c < 100.0 => format!("${c:.2}"),
        c if c < 10_000.0 => format!("${c:.0}"),
        c if c < 1e6 => format!("${:.1}k", c / 1e3),
        c => format!("${:.1}M", c / 1e6),
    }
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
pub(crate) struct Local {
    pub month: i64,
    pub day: i64,
    pub weekday: i64,
    pub hour: i64,
    pub minute: i64,
    pub second: i64,
}

pub(crate) fn local(epoch: i64) -> Local {
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
    fn no_row_of_the_card_is_wider_than_the_card() {
        // Long names and details in a narrow pane: the bar gives way to
        // its least, then the detail and the name are cut.
        let mut view = View::new(scope());
        let (_, window) = view.query(1_800_000_000_000);
        view.loaded(
            window,
            Ok(json!({
                "turns": 2, "input_tokens": 2000, "cached_input_tokens": 1000, "output_tokens": 200,
                "bots": [
                    {"bot": "p.the-refunds-for-every-region-at-once", "turns": 1, "input_tokens": 1000, "cached_input_tokens": 500, "output_tokens": 100},
                    {"bot": "p.lead", "turns": 1, "input_tokens": 1000, "cached_input_tokens": 500, "output_tokens": 100},
                ],
                "other_bots": null,
                "models": [
                    {"model": "bedrock/us.anthropic.claude-opus-5-5-v1:0", "turns": 1, "input_tokens": 1000, "cached_input_tokens": 500, "output_tokens": 100},
                    {"model": "a-gateway-with-a-long-name/an-unlisted-model-with-a-long-id", "turns": 1, "input_tokens": 1000, "cached_input_tokens": 500, "output_tokens": 100},
                ],
                "origins": [],
                "series": [],
            })),
            prices("\"bedrock/*\" = { input = 5, output = 25 }\n"),
        );
        // The sentence over the bars wraps as prose; a bar row would
        // wrap its share away from its bar.
        for width in 30..=100 {
            let card = view.card(width);
            let bars = card
                .into_iter()
                .filter(|line| text(std::slice::from_ref(line)).ends_with('%'));
            for line in bars {
                assert!(
                    line.width() <= width,
                    "{} columns in {width}: {:?}",
                    line.width(),
                    text(&[line])
                );
            }
        }
        // With room, the provider stays beside the model.
        let card = text(&view.card(100));
        assert!(card.contains("bedrock"), "{card}");
    }

    #[test]
    fn each_model_says_how_fast_it_answered_and_the_daemon_its_own_time() {
        let timed = |timed, timed_output, model_ms, first_ms| Spent {
            output: timed_output,
            timed,
            timed_output,
            model_ms,
            first_ms,
            ..Spent::default()
        };
        let rows: Vec<Row> = vec![
            (
                "Opus 5.5".into(),
                "anthropic".into(),
                timed(4, 6_000, 100_000, 5_600),
            ),
            (
                "GPT-6 Sol · fast".into(),
                "openai".into(),
                timed(2, 3_000, 20_000, 1_200),
            ),
            // Calls from before the daemon timed them say nothing.
            ("Haiku 5.5".into(), "anthropic".into(), timed(0, 0, 0, 0)),
        ];
        let total = Spent {
            timed: 6,
            daemon_ms: 54,
            ..Spent::default()
        };
        let card = text(&speeds(&rows, &total, 80));
        // In the order the models' spend lists them.
        assert_eq!(
            card.lines().skip(1).collect::<Vec<_>>(),
            [
                "Speed",
                "Opus 5.5             60 tok/s   1.4 s to first",
                "GPT-6 Sol · fast    150 tok/s  600 ms to first",
                "daemon                   9 ms  before each call",
            ]
        );
        assert!(speeds(&rows[2..], &total, 80).is_empty());
    }

    #[test]
    fn each_agent_says_where_its_turns_went() {
        // Two agents' finished turns: one waits on its model, the other on
        // its tools; the longer turn's bar is the longest.
        let mut view = View::new(scope());
        let (_, window) = view.query(1_800_000_000_000);
        let time = |turns, turn_ms, rounds, calls, tool_ms, model_ms, output| {
            json!({"turns": turns, "turn_ms": turn_ms, "model_rounds": rounds,
                "tool_calls": calls, "tool_ms": tool_ms, "paced_ms": 0, "summary_ms": 0,
                "daemon_ms": 0, "timed_rounds": rounds, "timed_output_tokens": output,
                "model_ms": model_ms, "first_token_ms": 0})
        };
        view.loaded(
            window,
            Ok(json!({
                "turns": 4, "input_tokens": 2000, "cached_input_tokens": 0, "output_tokens": 200,
                "time": time(4, 100_000, 20, 16, 30_000, 62_000, 3_100),
                "bots": [
                    {"bot": "p.docs", "turns": 2, "input_tokens": 1000, "cached_input_tokens": 0,
                     "output_tokens": 100, "time": time(2, 80_000, 18, 16, 30_000, 46_000, 1_400)},
                    {"bot": "p.lead", "turns": 2, "input_tokens": 1000, "cached_input_tokens": 0,
                     "output_tokens": 100, "time": time(2, 20_000, 2, 0, 0, 16_000, 1_700)},
                ],
                "other_bots": null, "models": [], "origins": [], "series": [],
            })),
            Vec::new(),
        );
        let lines = view.card(70);
        let card = text(&lines);
        let time: Vec<&str> = card
            .lines()
            .skip_while(|l| !l.starts_with("Time"))
            .collect();
        assert_eq!(time.len(), 4, "{card}");
        assert!(time[0].ends_with("a turn rounds  calls  tok/s"), "{card}");
        assert!(time[1].starts_with("docs"), "{card}");
        assert!(time[1].ends_with(" 40 s    9.0    8.0     30"), "{card}");
        assert!(time[2].ends_with(" 10 s    1.0    0.0    106"), "{card}");
        assert_eq!(
            time[3],
            "█ model 62%   █ tools 30%   █ waiting and the rest 8%"
        );
        // The longer turn's bar is longer, and no row is wider than the card.
        let filled = |row: &str| row.chars().filter(|c| "█▏▎▍▌▋▊▉".contains(*c)).count();
        assert!(filled(time[1]) > 3 * filled(time[2]), "{card}");
        for line in lines
            .iter()
            .filter(|l| text(std::slice::from_ref(l)).contains(" s "))
        {
            assert!(line.width() <= 70, "{card}");
        }
        assert_eq!(span(42_000), "42 s");
        assert_eq!(span(72_000), "1m 12s");
        assert_eq!(span(3_720_000), "1h 02m");
    }

    #[test]
    fn a_short_provider_is_shown_whole_when_it_fits() {
        // A binding named in two letters needs no cut, so it stays: it is
        // what tells one model's rows apart.
        let view = View::new(scope());
        let spent = Spent {
            fresh: 1000,
            ..Spent::default()
        };
        let rows: Vec<Row> = vec![
            ("Opus 5.5".into(), "gw".into(), spent),
            ("Opus 5.5".into(), "ap".into(), spent),
        ];
        let card = text(&view.bars("By model", &rows, &spent, 80, false));
        assert!(card.contains("Opus 5.5  gw  "), "{card}");
        assert!(card.contains("Opus 5.5  ap  "), "{card}");
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
                "models": [
                    {"model": "bedrock/us.anthropic.claude-opus-5-5-v1:0", "turns": 1, "input_tokens": 1000, "cached_input_tokens": 500, "output_tokens": 100},
                    {"model": "openai/gpt-6-sol", "turns": 1, "input_tokens": 1000, "cached_input_tokens": 1000, "output_tokens": 100},
                    {"model": "bedrock/global.anthropic.claude-opus-5-5-v1:0", "turns": 1, "input_tokens": 1000, "cached_input_tokens": 500, "output_tokens": 100},
                ],
                "origins": [
                    {"origin": null, "turns": 2, "input_tokens": 2000, "cached_input_tokens": 1500, "output_tokens": 200},
                    {"origin": "commons", "turns": 1, "input_tokens": 1000, "cached_input_tokens": 500, "output_tokens": 100},
                ],
                "series": [
                    {"at_ms": since, "turns": 1, "input_tokens": 1000, "cached_input_tokens": 500, "output_tokens": 100},
                    {"at_ms": since + 23 * bucket, "turns": 2, "input_tokens": 2000, "cached_input_tokens": 1500, "output_tokens": 200},
                ],
            })),
            Vec::new(),
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
        // Each model by its name, its provider muted beside it; two
        // regions' ids of one model are one row.
        let models: Vec<&str> = (card.lines())
            .skip_while(|line| *line != "By model")
            .skip(1)
            .take_while(|line| !line.is_empty())
            .collect();
        assert_eq!(models.len(), 2, "{card}");
        assert!(models[0].starts_with("Opus 5.5   bedrock  █"), "{card}");
        assert!(models[1].starts_with("GPT 6 Sol  openai   █"), "{card}");
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

    fn prices(sheet: &str) -> Vec<(String, Prices)> {
        let dir = std::env::temp_dir().join(format!("sonny-prices-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{:x}.toml", sheet.len()));
        std::fs::write(&path, sheet).unwrap();
        let config = agent_client::config::Config {
            prices: Some(path.display().to_string()),
            ..Default::default()
        };
        let prices = agent_client::config::prices(&config).unwrap();
        let _ = std::fs::remove_file(&path);
        prices
    }

    #[test]
    fn each_model_is_priced_from_the_sheet_and_one_it_does_not_name_says_so() {
        let mut view = View::new(scope());
        let (_, window) = view.query(1_800_000_000_000);
        let usage = json!({
            "turns": 2, "input_tokens": 3_000_000, "cached_input_tokens": 1_000_000, "output_tokens": 300_000,
            "bots": [{"bot": "p.lead", "turns": 2, "input_tokens": 3_000_000, "cached_input_tokens": 1_000_000, "output_tokens": 300_000}],
            "other_bots": null,
            "models": [
                // Two regions' ids of one model, each priced, on one row.
                {"model": "bedrock/us.anthropic.claude-opus-5-5-v1:0", "turns": 1, "input_tokens": 1_000_000,
                 "cached_input_tokens": 500_000, "cache_write_tokens": 200_000, "cache_write_1h_tokens": 100_000, "output_tokens": 100_000},
                {"model": "bedrock/global.anthropic.claude-opus-5-5-v1:0", "turns": 1, "input_tokens": 1_000_000,
                 "cached_input_tokens": 500_000, "cache_write_tokens": 0, "cache_write_1h_tokens": 0, "output_tokens": 100_000},
                {"model": "openai/gpt-6-sol", "turns": 1, "input_tokens": 1_000_000, "cached_input_tokens": 0, "output_tokens": 100_000},
            ],
            "origins": [],
            "series": [],
        });
        // Without a sheet, tokens alone, as before.
        view.loaded(window, Ok(usage.clone()), Vec::new());
        let card = text(&view.card(80));
        assert!(!card.contains('$'), "{card}");
        view.loaded(
            window,
            Ok(usage),
            prices(
                "\"bedrock/*anthropic.claude-opus-5*\" = \
                 { input = 5, cached = 0.5, cache_write = 6.25, cache_write_1h = 10, output = 25 }\n",
            ),
        );
        let card = text(&view.card(80));
        // 0.3M fresh, 0.5M read, 0.1M written for five minutes and 0.1M for
        // an hour, 0.1M out; then 0.5M fresh, 0.5M read, 0.1M out.
        let opus = 0.3 * 5.0
            + 0.5 * 0.5
            + 0.1 * 6.25
            + 0.1 * 10.0
            + 0.1 * 25.0
            + 0.5 * 5.0
            + 0.5 * 0.5
            + 0.1 * 25.0;
        assert_eq!(money(opus), "$11.12");
        assert!(
            card.contains(
                "3.3M tokens · 2 turns · 33% of input from cache · $11.12 · 1 model not priced"
            ),
            "{card}"
        );
        let models: Vec<&str> = (card.lines())
            .skip_while(|line| *line != "By model")
            .skip(1)
            .take_while(|line| !line.is_empty())
            .collect();
        assert_eq!(models.len(), 2, "{card}");
        assert!(models[0].starts_with("Opus 5.5"), "{card}");
        assert!(models[0].ends_with("  $11.12"), "{card}");
        assert!(models[1].ends_with("no price"), "{card}");
        // Agents are not priced: a turn may span models.
        let lead = card.lines().find(|line| line.starts_with("lead ")).unwrap();
        assert!(lead.ends_with('%'), "{card}");
    }

    #[test]
    fn each_speed_is_a_row_of_its_own_priced_at_its_rates() {
        let mut view = View::new(scope());
        let (_, window) = view.query(1_800_000_000_000);
        let usage = json!({
            "turns": 2, "input_tokens": 2_000_000, "cached_input_tokens": 0, "output_tokens": 200_000,
            "bots": [{"bot": "p.lead", "turns": 2, "input_tokens": 2_000_000, "cached_input_tokens": 0, "output_tokens": 200_000}],
            "other_bots": null,
            "models": [
                {"model": "bedrock/us.openai.gpt-6.1-sol", "speed": "ultrafast", "turns": 1,
                 "input_tokens": 1_000_000, "cached_input_tokens": 0, "output_tokens": 100_000},
                {"model": "bedrock/us.openai.gpt-6.1-sol", "speed": "standard", "turns": 1,
                 "input_tokens": 1_000_000, "cached_input_tokens": 0, "output_tokens": 100_000},
            ],
            "origins": [],
            "series": [],
        });
        view.loaded(
            window,
            Ok(usage),
            prices(
                "\"bedrock/*gpt-6.1-sol\" = { input = 2, output = 10, \
                 ultrafast = { input = 12, output = 60 } }\n",
            ),
        );
        let card = text(&view.card(80));
        let models: Vec<&str> = (card.lines())
            .skip_while(|line| *line != "By model")
            .skip(1)
            .take_while(|line| !line.is_empty())
            .collect();
        assert_eq!(models.len(), 2, "{card}");
        // 1M in and 0.1M out at each tier's own rates; equal spend, so by
        // name.
        assert!(models[0].starts_with("GPT 6.1 Sol  "), "{card}");
        assert!(models[0].ends_with("  $3.00"), "{card}");
        assert!(models[1].starts_with("GPT 6.1 Sol · ultrafast"), "{card}");
        assert!(models[1].ends_with("  $18.00"), "{card}");
        assert!(card.contains("$21.00"), "{card}");
    }

    #[test]
    fn a_cost_reads_in_a_few_characters() {
        for (cost, text) in [
            (0.0, "$0"),
            (0.004, "<$0.01"),
            (0.42, "$0.42"),
            (99.5, "$99.50"),
            (123.4, "$123"),
            (12_345.0, "$12.3k"),
            (2_500_000.0, "$2.5M"),
        ] {
            assert_eq!(money(cost), text);
            assert!(text.len() <= COST);
        }
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
