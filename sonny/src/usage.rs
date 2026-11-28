//! `/usage`: the tokens the project's agents spent, or every agent's, in
//! the last day, week or month: in all, over time as stacked bars, how
//! the tasks went (how many finished without the person, how often they
//! stepped in and how, how many are done (closed), without them by how long
//! a task worked and by its role, and that share the period before), by
//! agent (or project), by model, and by what started the turns: the
//! commons, a trigger, the margin, or a prompt; and last the Daemon block,
//! how much the daemon got through in the period and, from its `stats`
//! read on a connection of its own beside the rest, how it runs now. Daemon reads each time the
//! view changes and every minute while it is open: `usage`, and for the
//! tasks `bots`, `usage` by their names, the person's `messages` and the
//! calls they decided in `audit`, this period and the one before, and each
//! task's first message once (it never changes, so it is kept beside the
//! store for every window). They read every matching turn, so they are not
//! asked for often. With the person's price sheet (`~/.agent/prices.toml`,
//! or the file config's `prices` names), each model's cost and their sum,
//! priced here from the sheet as it stands at each read; the daemon counts
//! tokens only.
use crate::background::Scope;
use agent_client::config::Prices;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    style::{Color, Style, Stylize},
    text::{Line, Span},
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
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

    /// The period before it, as a share of it is compared with.
    fn before(self) -> &'static str {
        match self {
            Period::Day => "the day before",
            Period::Week => "the week before",
            Period::Month => "the month before",
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
    /// The daemon's own state from its last two `stats` reads, which a
    /// read beside the window fills in ([`stats`]).
    daemon: Arc<std::sync::Mutex<Readings>>,
}

struct Read {
    since: i64,
    bucket: i64,
    buckets: usize,
    /// When it came back.
    now: i64,
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
            daemon: Arc::default(),
        }
    }

    /// Whether it should be read again now.
    pub fn due(&self) -> bool {
        self.at.is_none_or(|at| at.elapsed() >= FRESH)
    }

    /// The `usage` request for what it shows, at `now`, and the window
    /// it asks for. It asks by the hour whatever the period, so the
    /// busiest hour is one rule for all three; the chart sums the hours
    /// into its bars.
    pub fn query(&self, now: i64) -> (Value, (i64, i64, usize)) {
        let (since, bucket, buckets) = self.period.window(now);
        let mut query = json!({"since_ms": since, "bucket_ms": HOUR, "limit": 256});
        if !self.all {
            query["name"] = json!(self.scope.glob);
        }
        (query, (since, bucket, buckets))
    }

    /// What [`read`] asks the daemon for what it shows, at `now`, and the
    /// window it asks for.
    pub fn ask(&self, now: i64) -> (Ask, (i64, i64, usize)) {
        let (usage, window) = self.query(now);
        let ask = Ask {
            usage,
            name: (!self.all).then(|| self.scope.glob.clone()),
            since: window.0,
            now,
        };
        (ask, window)
    }

    /// What it shows: every project's or this one's, and over which
    /// period. A read asked for another is not shown.
    pub fn selection(&self) -> (bool, Period) {
        (self.all, self.period)
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
                    now: epoch_ms(),
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
            KeyCode::Esc => return Key::Close,
            // Its one value, the period, takes ← and →: it has no rows to
            // open, and a letter means one thing in every pane.
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

    /// The card at `width`, each agent a row is named for (not `own`,
    /// this window's) in the agents' color, and where, to open it.
    pub fn card_linked(
        &self,
        width: usize,
        own: &str,
    ) -> (Vec<Line<'static>>, crate::links::Agents) {
        let mut card = self.card(width);
        let mut links = crate::links::Agents::new();
        let Some(read) = self.read.as_ref().filter(|_| !self.all) else {
            return (card, links);
        };
        let names: Vec<(String, String)> = (read.usage["bots"].as_array().into_iter().flatten())
            .filter_map(|bot| bot["bot"].as_str())
            .filter(|bot| *bot != own)
            .map(|bot| (self.scope.short(bot), bot.to_owned()))
            .collect();
        for (at, line) in card.iter_mut().enumerate() {
            // A row of a bar or a time is named by its first words.
            let Some(first) = line.spans.first() else {
                continue;
            };
            let name = first.content.trim_end();
            let Some((_, bot)) = names.iter().find(|(short, _)| short == name) else {
                continue;
            };
            let pad = first.content[name.len()..].to_owned();
            let wide = name.width() as u16;
            let style = first.style;
            let name = Span::styled(name.to_owned(), style.fg(Color::Blue));
            line.spans.splice(0..1, [name, Span::styled(pad, style)]);
            links.push((at, 0, wide, bot.clone()));
        }
        (card, links)
    }

    /// The card at `width`.
    pub fn card(&self, width: usize) -> Vec<Line<'static>> {
        let whose = match self.all {
            true => "every project".to_owned(),
            false => self.scope.project.clone(),
        };
        // The pane's title names it: its first row says whose and when.
        let mut out = vec![Line::from(vec![
            Span::raw(whose.clone()).bold(),
            Span::raw(format!(" · {}", self.period.label())).dim(),
        ])];
        if let Some(error) = &self.error {
            out.push(Line::default());
            out.push(Line::from(vec![
                Span::raw("✗ ").fg(Color::Red),
                Span::raw(format!("could not read usage: {error}")),
            ]));
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
                Line::from("Each turn an agent takes counts here, in the project it started in.")
                    .dim(),
            );
            // A task can still work, or have ended, on a turn started
            // before the period.
            if let Some(tasks) = Tasks::of(&usage["tasks"]) {
                out.push(Line::default());
                out.extend(tasks.lines(read.period, width));
            }
            out.push(Line::default());
            out.extend(self.daemon(read, width));
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
        if let Some(tasks) = Tasks::of(&usage["tasks"]) {
            out.extend(tasks.lines(read.period, width));
        }
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
        out.push(Line::default());
        out.extend(self.daemon(read, width));
        out
    }

    /// The Daemon block: what it did in the period, where its finished
    /// turns' time went, and its state now, from its last two `stats`
    /// reads, the whole daemon's whatever the card shows.
    fn daemon(&self, read: &Read, width: usize) -> Vec<Line<'static>> {
        let usage = &read.usage;
        let count = |value: &Value| value.as_u64().unwrap_or(0);
        let turns = count(&usage["turns"]);
        // The calls and the time are the finished turns' with a timed
        // call, as `usage` keeps them in `time`.
        let time = &usage["time"];
        let timed = count(&time["turns"]);
        let (model_calls, tool_calls) = (count(&time["timed_rounds"]), count(&time["tool_calls"]));
        let mut head = format!("  {} turns", thousands(turns));
        if timed > 0 {
            head.push_str(&format!(
                " · {} model calls · {} tool calls",
                thousands(model_calls),
                thousands(tool_calls)
            ));
        }
        let mut out = vec![fit(
            vec![Span::raw("Daemon").bold(), Span::raw(head).dim()],
            width,
        )];
        if turns > 0 {
            out.push(Line::default());
            let hours = ((read.now - read.since) as f64 / HOUR as f64).max(1.0);
            let busiest = (usage["series"].as_array().into_iter().flatten())
                .map(|hour| count(&hour["turns"]))
                .max()
                .unwrap_or(0);
            // The names give way first, down to the longest, then the
            // columns from the right, each whole.
            let columns = [
                (13, "in the period"),
                (14, "each hour"),
                (16, "busiest hour"),
            ];
            let name = (width.saturating_sub(43)).clamp(15, NAME);
            let kept = (1..=3)
                .rev()
                .find(|n| name + columns[..*n].iter().map(|(w, _)| w).sum::<usize>() <= width)
                .unwrap_or(1);
            let heads: String = (columns[..kept].iter())
                .map(|(w, head)| format!("{head:>w$}"))
                .collect();
            out.push(fit(
                vec![
                    Span::raw(format!("{:<name$}", "What it did")).bold(),
                    Span::raw(heads).dim(),
                ],
                width,
            ));
            let mut did = vec![("turns", turns, Some(busiest))];
            if timed > 0 {
                did.push(("model calls", model_calls, None));
                did.push(("tool calls", tool_calls, None));
            }
            for (label, sum, most) in did {
                let cells = [
                    thousands(sum),
                    per(sum as f64 / hours),
                    most.map(thousands).unwrap_or_default(),
                ];
                let mut row = format!("  {label:<w$}", w = name - 2);
                for ((w, _), cell) in columns[..kept].iter().zip(cells) {
                    row.push_str(&format!("{cell:>w$}"));
                }
                out.push(fit(vec![Span::raw(row.trim_end().to_owned())], width));
            }
            if timed > 0 && timed < turns {
                let note = format!(
                    "  calls are counted in the {} turns that ended",
                    thousands(timed)
                );
                out.push(fit(vec![Span::raw(note).dim()], width));
            }
        }
        // Each part of the finished turns' time, as a share of it; what
        // none of them records, its own row; never scaled to make 100.
        let whole = count(&time["turn_ms"]);
        if timed > 0 && whole > 0 {
            let mut parts = vec![
                ("the model", count(&time["model_ms"])),
                ("tools", count(&time["tool_ms"])),
                ("waiting on a provider's limit", count(&time["paced_ms"])),
                ("summaries", count(&time["summary_ms"])),
                ("the daemon", count(&time["daemon_ms"])),
            ];
            let rest = whole.saturating_sub(parts.iter().map(|(_, ms)| ms).sum());
            if rest > 0 {
                parts.push(("the rest", rest));
            }
            out.push(Line::default());
            out.push(fit(
                vec![Span::raw("Where a turn's time went").bold()],
                width,
            ));
            let name = (NAME - 2).min(width.saturating_sub(2 + 4 + 6)).max(8);
            let room = width.saturating_sub(2 + name + 6).min(22);
            for (part, ms) in parts {
                let mut spans = vec![Span::raw(format!(
                    "  {:<name$}",
                    crate::transcript::cut(part, name)
                ))];
                spans.extend(meter(ms, whole, room));
                spans.push(Span::raw(format!(" {:>5}", share(ms, whole))));
                out.push(fit(spans, width));
            }
        }
        out.push(Line::default());
        out.extend(self.readings().lines(width));
        out
    }

    /// What the daemon's last two `stats` reads found, as they stand.
    fn readings(&self) -> Readings {
        self.daemon
            .lock()
            .map(|kept| kept.clone())
            .unwrap_or_default()
    }

    /// Where [`stats`] keeps what it reads for this view, and whether a
    /// read for it should start now: one at a time.
    pub fn stats_due(&self) -> Option<Arc<std::sync::Mutex<Readings>>> {
        let mut kept = self.daemon.lock().ok()?;
        if kept.asked {
            return None;
        }
        kept.asked = true;
        Some(self.daemon.clone())
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

    /// The line under the card: the keys it takes.
    pub fn keys(&self) -> String {
        let scope = match self.all {
            true => "a this project",
            false => "a every project",
        };
        let cached = match self.cached {
            true => "c hide cached",
            false => "c show cached",
        };
        let pair = |said: &'static str| said.split_once(' ').unwrap_or((said, ""));
        let (scope, cached) = (pair(scope), pair(cached));
        (crate::hint::Keys::new().key("←→", "period"))
            .key(scope.0, scope.1)
            .key(cached.0, cached.1)
            .esc(crate::hint::Esc::Close)
            .text()
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

/// The name column of the Daemon block's rows.
const NAME: usize = 32;

/// What the daemon counts since it started that the Daemon block reads
/// as a rate: requests answered, writes the store committed, and events
/// sent to the sessions that follow them.
const RATES: [&str; 3] = ["requests", "store writes", "events sent"];

/// What one `stats` read said of the daemon: how long it had been up,
/// each of [`RATES`] and the processor time it spent since it started,
/// and its levels then. A figure the daemon reported as null, or a limit
/// it has none of (0), is none.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Reading {
    uptime_ms: u64,
    counts: [Option<u64>; 3],
    cpu_ms: Option<u64>,
    turns: Option<u64>,
    turn_limit: Option<u64>,
    paced: Option<u64>,
    commands: Option<u64>,
    command_limit: Option<u64>,
    rss: Option<u64>,
    peak: Option<u64>,
}

impl Reading {
    /// From a `stats` reply; none without its uptime, which every rate is
    /// read against.
    pub fn of(stats: &Value) -> Option<Self> {
        let process = &stats["process"];
        let requests = (stats["commands"]["ops"].as_object()).map(|ops| {
            ops.values()
                .map(|op| op["count"].as_u64().unwrap_or(0))
                .sum()
        });
        let limit = |value: &Value| value.as_u64().filter(|limit| *limit > 0);
        Some(Self {
            uptime_ms: process["uptime_ms"].as_u64()?,
            counts: [
                requests,
                stats["store"]["groups"]["jobs"].as_u64(),
                stats["hub"]["delivered"].as_u64(),
            ],
            cpu_ms: (process["cpu_user_ms"].as_u64())
                .zip(process["cpu_system_ms"].as_u64())
                .map(|(user, system)| user + system),
            turns: stats["active_turns"].as_u64(),
            turn_limit: limit(&stats["active_limit"]),
            paced: stats["paced_turns"].as_u64(),
            commands: stats["running_processes"].as_u64(),
            command_limit: limit(&stats["process_limit"]),
            rss: process["rss_bytes"].as_u64(),
            peak: process["peak_rss_bytes"].as_u64(),
        })
    }

    /// Whether the daemon started again after `before` was read: it has
    /// been up less long, or a count fell.
    fn restarted(&self, before: &Self) -> bool {
        let fell = |now: Option<u64>, then: Option<u64>| now.zip(then).is_some_and(|(n, t)| n < t);
        self.uptime_ms < before.uptime_ms
            || (self.counts.iter().zip(before.counts)).any(|(now, then)| fell(*now, then))
            || fell(self.cpu_ms, before.cpu_ms)
    }
}

/// The daemon's last two `stats` reads, the earlier none until a second
/// lands or once the daemon started again between them; the last read's
/// error; and whether a read is under way.
#[derive(Clone, Debug, Default)]
pub struct Readings {
    last: Option<Reading>,
    before: Option<Reading>,
    error: Option<String>,
    asked: bool,
}

impl Readings {
    /// What a `stats` read came back with.
    pub fn came(&mut self, read: Result<Value, String>) {
        self.asked = false;
        let reading = match read {
            Ok(stats) => Reading::of(&stats).ok_or_else(|| "no uptime".to_owned()),
            Err(error) => Err(error),
        };
        match reading {
            Ok(reading) => {
                self.error = None;
                self.before = self.last.take().filter(|last| !reading.restarted(last));
                self.last = Some(reading);
            }
            Err(error) => self.error = Some(error),
        }
    }

    /// The Right now table at `width`.
    fn lines(&self, width: usize) -> Vec<Line<'static>> {
        let mut head = vec![
            Span::raw("Right now").bold(),
            Span::raw("  the whole daemon").dim(),
        ];
        let Some(last) = &self.last else {
            head.push(
                Span::raw(match &self.error {
                    Some(error) => format!(" · could not read it: {error}"),
                    None => " · reading…".to_owned(),
                })
                .dim(),
            );
            return vec![fit(head, width)];
        };
        head.push(Span::raw(format!(" · up {}", up(last.uptime_ms))).dim());
        let mut out = vec![fit(head, width)];
        let mut rows: Vec<(&str, String, String)> = Vec::new();
        let mut row = |name, value, note| rows.push((name, value, note));
        // A count's rate between the last two reads, and since the daemon
        // started; only that until a second read lands.
        let up_s = last.uptime_ms as f64 / 1000.0;
        let between = |now: Option<u64>, then: Option<u64>| {
            let before = self.before.as_ref()?;
            let ms = last
                .uptime_ms
                .checked_sub(before.uptime_ms)
                .filter(|ms| *ms > 0)?;
            Some((now? - then?) as f64 * 1000.0 / ms as f64)
        };
        for (at, name) in RATES.iter().enumerate() {
            let Some(sum) = last.counts[at] else { continue };
            let since = sum as f64 / up_s.max(0.001);
            let then = self.before.as_ref().and_then(|before| before.counts[at]);
            let (value, note) = match between(Some(sum), then) {
                Some(rate) => (rate, format!("{} a second since it started", per(since))),
                None => (since, "since it started".to_owned()),
            };
            row(name, format!("{} a second", per(value)), note);
        }
        if let Some(turns) = last.turns {
            let value = match last.turn_limit {
                Some(limit) => format!("{turns} of {limit}"),
                None => turns.to_string(),
            };
            let note = match last.paced {
                Some(paced) if paced > 0 => format!("{paced} more wait on a provider's limit"),
                _ => String::new(),
            };
            row("turns running", value, note);
        }
        if let Some(commands) = last.commands {
            let value = match last.command_limit {
                Some(limit) => format!("{commands} of {limit}"),
                None => commands.to_string(),
            };
            row("commands running", value, String::new());
        }
        // The processor time it spent as a share of one core's, read as
        // the counts are.
        if let Some(cpu) = last.cpu_ms {
            let since = cpu as f64 * 100.0 / last.uptime_ms.max(1) as f64;
            let then = self.before.as_ref().and_then(|before| before.cpu_ms);
            let (value, note) = match between(Some(cpu), then) {
                Some(share) => (share / 10.0, format!("{} since it started", core(since))),
                None => (since, "since it started".to_owned()),
            };
            row("processor", format!("{} of one core", core(value)), note);
        }
        if let Some(rss) = last.rss {
            // The kernel counts the peak and the resident set apart: the
            // most is never less than it uses now.
            let note = last
                .peak
                .map(|peak| format!("most {}", megabytes(peak.max(rss))));
            row("memory", megabytes(rss), note.unwrap_or_default());
        }
        // The names give way to the notes, down to the longest name; a
        // note that still does not fit is left out whole.
        let notes = (rows.iter().map(|(_, _, note)| note.width()))
            .max()
            .unwrap_or(0);
        let name = (width.saturating_sub(2 + 15 + 3 + notes)).clamp(17, NAME - 5);
        for (label, value, note) in rows {
            let mut spans = vec![Span::raw(format!("  {label:<name$}{value:>15}"))];
            if !note.is_empty() && 2 + name + 15 + 3 + note.width() <= width {
                spans.push(Span::raw(format!("   {note}")).dim());
            }
            out.push(fit(spans, width));
        }
        out
    }
}

/// The daemon's state, read with `stats` on a connection of its own to
/// `socket`, so its reply never waits behind the window's reads, into
/// `kept`; `woken` once it is there.
pub async fn stats(
    socket: PathBuf,
    kept: Arc<std::sync::Mutex<Readings>>,
    woken: Arc<tokio::sync::Notify>,
) {
    let read = match agent_client::Client::connect(&socket).await {
        Ok((client, _events)) => {
            (client.request("stats", json!({})).await).map_err(|error| error.to_string())
        }
        Err(error) => Err(error.to_string()),
    };
    if let Ok(mut kept) = kept.lock() {
        kept.came(read);
    }
    woken.notify_one();
}

/// `spans` cut to `width`: the span that runs over it cut short, those
/// after it left out.
fn fit(spans: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let mut used = 0;
    let mut out = Vec::new();
    for span in spans {
        let wide = span.content.width();
        if used + wide <= width {
            used += wide;
            out.push(span);
            continue;
        }
        let room = width - used;
        if room > 1 {
            let cut = crate::transcript::cut(span.content.trim_end(), room);
            out.push(Span::styled(cut, span.style));
        }
        break;
    }
    Line::from(out)
}

/// How many of something a unit of time, in a few characters: whole from
/// ten on, to a tenth under it.
fn per(n: f64) -> String {
    match n {
        n if n >= 9.95 => thousands(n.round() as u64),
        n if n > 0.0 && n < 0.05 => "<0.1".to_owned(),
        n => format!("{n:.1}").trim_end_matches(".0").to_owned(),
    }
}

/// A share of one core, as a whole percent, or under one when it is.
fn core(percent: f64) -> String {
    match percent {
        p if p > 0.0 && p < 0.5 => "<1%".to_owned(),
        p => format!("{:.0}%", p),
    }
}

/// `part` of `of` as a whole percent, or under one when it is some.
fn share(part: u64, of: u64) -> String {
    match percent(part, of).as_str() {
        "0%" if part > 0 => "<1%".to_owned(),
        said => said.to_owned(),
    }
}

/// Bytes in megabytes, or gigabytes from a thousand of them.
fn megabytes(bytes: u64) -> String {
    let mb = bytes as f64 / 1_048_576.0;
    match mb {
        mb if mb < 1000.0 => format!("{} MB", mb.round() as u64),
        mb => format!("{:.1} GB", mb / 1024.0),
    }
}

/// How long the daemon has been up, in its two largest units: 45s, 12m,
/// 3h 20m, 2d 4h.
fn up(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86_400 => format!("{}h {}m", s / 3600, (s % 3600) / 60),
        _ => format!("{}d {}h", s / 86_400, (s % 86_400) / 3600),
    }
}

fn thousands(n: u64) -> String {
    crate::background::thousands(n)
}

fn epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
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

/// The longest a task worked, in milliseconds, for each length it is
/// counted under: under half an hour, half an hour to two hours, longer.
const LENGTHS: [(u64, &str); 3] = [
    (30 * 60_000, "under 30 min"),
    (2 * 3_600_000, "30 min to 2 h"),
    (u64::MAX, "over 2 h"),
];

/// How the tasks of a period went, as the Tasks section draws it: of the
/// agents other agents made that worked in the period (a workflow's
/// workers, leads and Sonny's own left out), how many still work, and of
/// those that ended, how many finished with nobody stepping in (no
/// message from the person, no call they decided), how the person stepped into the rest, how many are
/// done (closed), and the share without them by how long a task worked, by
/// its role, and in the period before.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Tasks {
    pub worked: u64,
    pub working: u64,
    pub ended: u64,
    pub alone: u64,
    /// Over the ended tasks: the person's messages to them, and calls the
    /// person decided.
    pub messages: u64,
    pub calls: u64,
    pub done: u64,
    /// Without the person and ended, for each of [`LENGTHS`].
    pub lengths: [(u64, u64); 3],
    /// Each role's name, without the person and ended, most ended first.
    pub roles: Vec<(String, u64, u64)>,
    /// Without the person and ended, the period before.
    pub before: Option<(u64, u64)>,
}

/// One agent another made, as [`read`] lists it.
struct Made<'a> {
    bot: &'a str,
    /// None when a turn went untimed.
    work_ms: Option<u64>,
    running: bool,
    /// Closed, as the daemon lists it: done once no turn runs.
    closed: bool,
    messages: u64,
    calls: u64,
    side: bool,
    /// The profile it was made in, which the daemon keeps on it.
    profile: Option<&'a str>,
}

impl<'a> Made<'a> {
    fn of(row: &'a Value) -> Option<Self> {
        let bot = row["bot"].as_str()?;
        let count = |key: &str| row[key].as_u64().unwrap_or(0);
        Some(Self {
            bot,
            work_ms: row["work_ms"].as_u64(),
            running: row["running"] == true,
            closed: row["closed"] == true,
            messages: count("person_messages"),
            calls: count("person_calls"),
            side: crate::side_copy(&json!({
                "name": bot, "created_by": row["created_by"], "expire_ms": row["expire_ms"],
            }))
            .is_some(),
            profile: row["profile"].as_str(),
        })
    }
    /// Whether it is a task: not a workflow's worker, a lead, a side chat,
    /// or one of Sonny's own.
    fn task(&self) -> bool {
        !crate::project::is_internal(self.bot)
            && !crate::project::is_lead(self.bot)
            && !crate::workflow::is_worker(self.bot)
            && !self.side
    }
    /// Whether the person stepped in.
    fn stepped(&self) -> bool {
        self.messages + self.calls > 0
    }
}

impl Tasks {
    /// The tasks among `made`, each one done once closed, and its role the
    /// profile it was made in.
    pub fn tally(made: &[Value]) -> Self {
        let mut tasks = Self::default();
        for made in made.iter().filter_map(Made::of).filter(Made::task) {
            tasks.worked += 1;
            if made.running {
                tasks.working += 1;
                continue;
            }
            tasks.ended += 1;
            let alone = !made.stepped() as u64;
            tasks.alone += alone;
            tasks.messages += made.messages;
            tasks.calls += made.calls;
            tasks.done += crate::resolved::done(made.closed, made.running) as u64;
            // A task with an untimed turn has no length to count it by.
            if let Some(work_ms) = made.work_ms {
                let length = (LENGTHS.iter())
                    .position(|(most, _)| work_ms < *most)
                    .unwrap_or(LENGTHS.len() - 1);
                tasks.lengths[length].0 += alone;
                tasks.lengths[length].1 += 1;
            }
            let role = made.profile.unwrap_or("no role").to_owned();
            match tasks.roles.iter_mut().find(|(name, ..)| *name == role) {
                Some((_, without, of)) => {
                    *without += alone;
                    *of += 1;
                }
                None => tasks.roles.push((role, alone, 1)),
            }
        }
        tasks
            .roles
            .sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
        tasks
    }

    /// Of the tasks among `made` of the period before, those that ended
    /// without the person, and those that ended.
    pub fn before(made: &[Value]) -> (u64, u64) {
        let ended: Vec<Made> = (made.iter().filter_map(Made::of))
            .filter(|made| made.task() && !made.running)
            .collect();
        let alone = ended.iter().filter(|made| !made.stepped()).count();
        (alone as u64, ended.len() as u64)
    }

    /// As kept in the usage read; none when no task worked.
    fn of(kept: &Value) -> Option<Self> {
        serde_json::from_value::<Self>(kept.clone())
            .ok()
            .filter(|tasks| tasks.worked > 0)
    }

    /// The section at `width`, for `period`: its head, then, once a task
    /// ended, each share as a bar.
    fn lines(&self, period: Period, width: usize) -> Vec<Line<'static>> {
        let mut head = format!("  {} worked in the {}", self.worked, period.label());
        if self.working > 0 {
            head.push_str(&format!(" · {} still working", self.working));
        }
        let mut out = vec![Line::from(vec![
            Span::raw("Tasks").bold(),
            Span::raw(crate::transcript::cut(&head, width.saturating_sub(5))).dim(),
        ])];
        if self.ended == 0 {
            out.push(Line::default());
            return out;
        }
        // The name gives way first, down to its least, then the bar.
        let name_width = 21.min(width.saturating_sub(SHARE + 2 + 4)).max(8);
        let room = width.saturating_sub(name_width + 2 + SHARE).min(30);
        let share = |name: &str, part: u64, of: u64, indent: usize| {
            let name = format!("{}{name}", " ".repeat(indent));
            let mut spans = vec![Span::raw(format!(
                "{:<name_width$}  ",
                crate::transcript::cut(&name, name_width)
            ))];
            spans.extend(meter(part, of, room));
            spans.push(Span::raw(format!(" {part:>4} of {of:<4}")));
            spans.push(Span::raw(format!(" {:>4}", percent(part, of))).dim());
            spans
        };
        let mut alone = share("Finished without you", self.alone, self.ended, 0);
        if let Some((part, of)) = self.before.filter(|(_, of)| *of > 0) {
            let was = format!("  was {} {}", percent(part, of), period.before());
            let used: usize = alone.iter().map(|span| span.content.width()).sum();
            if used + was.width() <= width {
                alone.push(Span::raw(was).dim());
            }
        }
        out.push(Line::from(alone));
        let stepped = self.ended - self.alone;
        out.push(Line::from(share("You stepped in", stepped, self.ended, 0)));
        if stepped > 0 {
            let mut how = Vec::new();
            if self.messages > 0 {
                how.push(format!("you wrote {}", self.messages));
            }
            if self.calls > 0 {
                how.push(format!("calls you decided {}", self.calls));
            }
            let how = format!("  {}", how.join(" · "));
            out.push(Line::from(crate::transcript::cut(&how, width)).dim());
        }
        out.push(Line::from(share("Done", self.done, self.ended, 0)));
        out.push(Line::default());
        out.push(
            Line::from(crate::transcript::cut(
                "Without you, by how long they worked",
                width,
            ))
            .bold(),
        );
        for ((alone, of), (_, name)) in self.lengths.iter().zip(LENGTHS) {
            if *of > 0 {
                out.push(Line::from(share(name, *alone, *of, 2)));
            }
        }
        // Roles only once there are two to tell apart.
        if self.roles.len() > 1 {
            out.push(Line::default());
            out.push(Line::from(crate::transcript::cut("Without you, by role", width)).bold());
            for (role, alone, of) in self.roles.iter().take(LISTED) {
                out.push(Line::from(share(role, *alone, *of, 2)));
            }
        }
        out.push(Line::default());
        out
    }
}

/// The columns a share takes after its bar: ` 15 of 21    71%`.
const SHARE: usize = 1 + 4 + 4 + 4 + 1 + 4;

/// `part` of `of` as a whole percent, rounded.
fn percent(part: u64, of: u64) -> String {
    format!("{}%", (part * 200 + of).checked_div(of * 2).unwrap_or(0))
}

/// `part` of `of` as a bar `room` columns long: filled, then the rest
/// faint.
fn meter(part: u64, of: u64, room: usize) -> Vec<Span<'static>> {
    let filled = ((part as usize * room * 2 + of as usize) / (of as usize * 2).max(1)).min(room);
    vec![
        Span::raw("█".repeat(filled)).fg(Color::Blue),
        Span::raw("░".repeat(room - filled)).fg(Color::DarkGray),
    ]
}

/// What `/usage` reads for a selection: its `usage` request, and the
/// tasks it covers from `since` to `now` and over the period before.
pub struct Ask {
    pub usage: Value,
    name: Option<String>,
    since: i64,
    now: i64,
}

/// The usage `ask` names, read with `window`, with how its tasks went
/// kept under `tasks` when all of it could be read.
pub async fn read(
    window: Arc<agent_client::Client>,
    socket: PathBuf,
    ask: Ask,
    store: PathBuf,
) -> Result<Value, String> {
    // The period's own usage is this period's for the tasks too.
    let (read, this) = tokio::sync::oneshot::channel();
    let usage = async {
        let usage = window.request("usage", ask.usage.clone()).await;
        let _ = read.send(usage.as_ref().ok().cloned());
        usage
    };
    let identity = window.identity().map(str::to_owned);
    let (usage, tasks) = tokio::join!(usage, tasks(&socket, &ask, store, identity, this));
    let mut usage = usage.map_err(|error| error.to_string())?;
    if let Some(tasks) = tasks {
        usage["tasks"] = serde_json::to_value(tasks).unwrap_or_default();
    }
    Ok(usage)
}

/// How the tasks `ask` covers went, read on connections of their own to
/// `socket`, so their replies never crowd the window's, and several, as the
/// daemon answers one connection's reads in turn: the tasks from `bots`,
/// their turns and time from the period's own `usage` (sent on by the
/// window as `usage`) and the period before's, or by their names when more
/// worked than one read lists, the person's messages from `messages` and
/// the calls they decided from `audit`, all asked at once; their done
/// marks and roles beside `store`, and each task's first message where an
/// earlier read found it ([`take_firsts`], of the daemon `identity`
/// names). None when any of it could not be read:
/// without every message and call, a task the person stepped into would
/// read as one that finished without them.
async fn tasks(
    socket: &std::path::Path,
    ask: &Ask,
    store: PathBuf,
    identity: Option<String>,
    usage: tokio::sync::oneshot::Receiver<Option<Value>>,
) -> Option<Tasks> {
    let (since, now) = (ask.since, ask.now);
    let before = (since - (now - since).max(1)).max(0);
    let mut connects = tokio::task::JoinSet::new();
    for _ in 0..=AT_ONCE {
        let socket = socket.to_owned();
        connects.spawn(async move { agent_client::Client::connect(&socket).await });
    }
    let (mut clients, mut _events) = (Vec::new(), Vec::new());
    while let Some(connected) = connects.join_next().await {
        let (client, events) = connected.ok()?.ok()?;
        clients.push(client);
        _events.push(events);
    }
    let scope = ask.name.clone().unwrap_or_else(|| "*".to_owned());
    let spent = async {
        // Each period's usage in one read when its bots fit one; else by
        // the tasks' names, once they are listed.
        let name = ask.name.as_deref();
        let (tasks, this, last) = tokio::join!(
            listed(&clients[0], name, before),
            async { Some(complete(&usage.await.ok()??)) },
            async { Some(complete(&spent_in(&clients[2], name, before, since).await?)) },
        );
        let tasks = tasks?;
        let names: Vec<&str> = tasks.iter().map(|task| task.bot.as_str()).collect();
        let (this, last) = tokio::join!(
            async {
                match this {
                    Some(Some(this)) => Some(this),
                    _ => spent(&clients, &names, since, now).await,
                }
            },
            async {
                match last {
                    Some(Some(last)) => Some(last),
                    _ => spent(&clients[AT_ONCE / 2..], &names, before, since).await,
                }
            },
        );
        Some((tasks, this?, last))
    };
    let (spent, wrote, decided) = tokio::join!(
        spent,
        async {
            let (kept, ident) = (store.clone(), identity.clone());
            let firsts = tokio::task::spawn_blocking(move || take_firsts(&kept, ident.as_deref()));
            let wrote = person(&clients, 1, Person::Messages, &scope, (before, now)).await?;
            without_briefs(&clients, wrote, &firsts.await.ok()?).await
        },
        person(&clients, 3, Person::Calls, &scope, (before, now)),
    );
    let ((tasks, this, last), (wrote, found), decided) = (spent?, wrote?, decided?);
    let (wrote, decided) = (split(&wrote, since), split(&decided, since));
    let tally = move || {
        if !found.is_empty() {
            keep_firsts(&store, identity.as_deref(), found);
        }
        let this = rows(&tasks, &this, &wrote.0, &decided.0, (since, now));
        let last = last.map(|last| rows(&tasks, &last, &wrote.1, &decided.1, (before, since)));
        let mut tasks = Tasks::tally(&this);
        tasks.before = last.as_deref().map(Tasks::before);
        tasks
    };
    tokio::task::spawn_blocking(tally).await.ok()
}

/// A task as `bots` lists it.
struct Listed {
    bot: String,
    bot_id: i64,
    /// When the turn it runs started; none when it runs none.
    running: Option<i64>,
    last: Option<i64>,
    /// When its last finished turn started and ended.
    ended: Option<(i64, i64)>,
    created_by: Value,
    expire_ms: Value,
    /// The profile it was made in.
    profile: Value,
    /// Closed, as the daemon lists it.
    closed: bool,
}

/// Every agent another made in scope `name` (all of them with none) that
/// ran since `since`.
async fn listed(
    client: &agent_client::Client,
    name: Option<&str>,
    since: i64,
) -> Option<Vec<Listed>> {
    let mut tasks = Vec::new();
    let mut after = json!("");
    loop {
        let mut query = json!({"after": after, "limit": 256});
        if let Some(name) = name {
            query["name"] = json!(name);
        }
        let page = client.request("bots", query).await.ok()?;
        for bot in page["bots"].as_array().into_iter().flatten() {
            let running = bot["running_turn"].is_i64();
            let ran = running
                || (bot["last"]["finished_ms"].as_i64()).is_some_and(|ended| ended >= since);
            if bot["created_by"].is_null() || !ran {
                continue;
            }
            tasks.push(Listed {
                bot: bot["name"].as_str().unwrap_or_default().to_owned(),
                bot_id: bot["bot_id"].as_i64().unwrap_or_default(),
                ended: (bot["last"]["started_ms"].as_i64())
                    .zip(bot["last"]["finished_ms"].as_i64()),
                running: running.then(|| bot["started_ms"].as_i64().unwrap_or(since)),
                last: bot["running_turn"]
                    .as_i64()
                    .or(bot["last"]["turn"].as_i64()),
                created_by: bot["created_by"].clone(),
                expire_ms: bot["expire_ms"].clone(),
                profile: bot["profile"].clone(),
                closed: crate::resolved::closed(bot),
            });
        }
        match &page["next_after"] {
            Value::Null => return Some(tasks),
            next => after = next.clone(),
        }
    }
}

/// The usage of each agent in scope `name` (every agent with none) from
/// `since` to `until`, in one read of as many as it lists; none when it
/// could not be read.
async fn spent_in(
    client: &agent_client::Client,
    name: Option<&str>,
    since: i64,
    until: i64,
) -> Option<Value> {
    let mut query = json!({"since_ms": since, "until_ms": until, "limit": 256});
    if let Some(name) = name {
        query["name"] = json!(name);
    }
    client.request("usage", query).await.ok()
}

/// What [`by_bot`] reads from a `usage` read, when it lists every agent
/// that worked.
fn complete(read: &Value) -> Option<HashMap<String, Worked>> {
    read["other_bots"].is_null().then(|| by_bot(read))
}

/// What an agent's turns in a period spent, as `usage` reads it.
#[derive(Clone, Copy, Default)]
struct Worked {
    /// The turns it started.
    turns: u64,
    /// Those that finished with timed calls, the only ones `usage` times.
    timed: u64,
    /// Their time from start to end.
    turn_ms: u64,
}

/// What each bot's turns spent, from a `usage` read.
fn by_bot(read: &Value) -> HashMap<String, Worked> {
    let count = |value: &Value| value.as_u64().unwrap_or(0);
    (read["bots"].as_array().into_iter().flatten())
        .filter_map(|bot| {
            let spent = Worked {
                turns: count(&bot["turns"]),
                timed: count(&bot["time"]["turns"]),
                turn_ms: count(&bot["time"]["turn_ms"]),
            };
            Some((bot["bot"].as_str()?.to_owned(), spent))
        })
        .collect()
}

/// The turns each of `names` started from `since` to `until`, and the time
/// its finished ones took, from `usage` asked for as many names at a time
/// as it takes, a few reads at once; none when a read failed.
async fn spent(
    clients: &[Arc<agent_client::Client>],
    names: &[&str],
    since: i64,
    until: i64,
) -> Option<HashMap<String, Worked>> {
    let mut chunks = names.chunks(256).enumerate();
    let mut reads = tokio::task::JoinSet::new();
    let mut spent = HashMap::new();
    loop {
        while reads.len() < AT_ONCE {
            let Some((at, names)) = chunks.next() else {
                break;
            };
            let (client, limit) = (clients[at % clients.len()].clone(), names.len());
            let query =
                json!({"names": names, "since_ms": since, "until_ms": until, "limit": limit});
            reads.spawn(async move { client.request("usage", query).await.ok() });
        }
        let Some(read) = reads.join_next().await else {
            return Some(spent);
        };
        spent.extend(by_bot(&read.ok()??));
    }
}

/// The connections the Tasks section reads on, the most `usage` reads
/// [`spent`] keeps asked at once for a period, and the slices [`person`]
/// reads side by side.
const AT_ONCE: usize = 4;

/// One thing the person did for an agent: its name and number, the
/// message's number (none for a call), and when.
struct Did {
    bot: String,
    bot_id: i64,
    message: Option<i64>,
    at: i64,
}

/// By agent number, how many of something the person did from `since` to
/// `now`, and in the period before.
type Split = (HashMap<i64, u64>, HashMap<i64, u64>);

/// How many of `done` fell to each agent, split at `since`.
fn split(done: &[Did], since: i64) -> Split {
    let mut split = Split::default();
    for did in done {
        let side = if did.at < since {
            &mut split.1
        } else {
            &mut split.0
        };
        *side.entry(did.bot_id).or_default() += 1;
    }
    split
}

/// The person's messages in `wrote` but each task's first: Sonny starts a
/// task with the person's words as its brief, which is no stepping in. A
/// task's first message is the first a read of its messages lists, before
/// the first of the person's here; none before it makes that one the
/// first. It never changes, so a task in `firsts` is read no more: the
/// rest are read side by side, and come back with what they found.
async fn without_briefs(
    clients: &[Arc<agent_client::Client>],
    wrote: Vec<Did>,
    firsts: &HashMap<i64, i64>,
) -> Option<(Vec<Did>, HashMap<i64, i64>)> {
    let mut first: HashMap<i64, (&str, i64)> = HashMap::new();
    // Leads, workflow workers and Sonny's own are no tasks.
    let task = |bot: &str| {
        !crate::project::is_lead(bot)
            && !crate::project::is_internal(bot)
            && !crate::workflow::is_worker(bot)
    };
    for did in wrote.iter().filter(|did| task(&did.bot)) {
        let Some(message) = did.message else { continue };
        let earliest = first.entry(did.bot_id).or_insert((&did.bot, message));
        earliest.1 = earliest.1.min(message);
    }
    let mut briefs = std::collections::HashSet::new();
    let mut reads = tokio::task::JoinSet::new();
    for (bot_id, (bot, message)) in first {
        if let Some(first) = firsts.get(&bot_id) {
            briefs.insert(*first);
            continue;
        }
        let client = clients[reads.len() % clients.len()].clone();
        let query = json!({"bot": bot, "before": message, "limit": 1});
        reads.spawn(async move {
            // A task gone since is no brief to leave out.
            let page = client.request("messages", query).await.ok()?;
            let first = match page["messages"].as_array()?.first() {
                None => message,
                Some(earlier) => earlier["message"].as_i64()?,
            };
            Some((bot_id, first))
        });
    }
    let mut found = HashMap::new();
    while let Some(read) = reads.join_next().await {
        if let Some((bot_id, first)) = read.ok()? {
            briefs.insert(first);
            found.insert(bot_id, first);
        }
    }
    let wrote = (wrote.into_iter())
        .filter(|did| !did.message.is_some_and(|message| briefs.contains(&message)))
        .collect();
    Some((wrote, found))
}

/// [`keep_firsts`]'s file, among the reads kept beside the store (a
/// leading dot, as no lead is named).
fn firsts_file(store: &std::path::Path) -> PathBuf {
    crate::reads_dir(store).join(".firsts.json")
}

/// Each task's first message by the task's number, as any window's
/// `/usage` found them in the daemon's store `identity` names: a store
/// made again at the same path numbers its agents and messages anew.
fn take_firsts(store: &std::path::Path, identity: Option<&str>) -> HashMap<i64, i64> {
    #[derive(serde::Deserialize)]
    struct Kept {
        store: String,
        firsts: HashMap<i64, i64>,
    }
    std::fs::read(firsts_file(store))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Kept>(&bytes).ok())
        .filter(|kept| kept.store == identity.unwrap_or(""))
        .map(|kept| kept.firsts)
        .unwrap_or_default()
}

/// Keep the first messages `found` for every window, with those kept.
fn keep_firsts(store: &std::path::Path, identity: Option<&str>, found: HashMap<i64, i64>) {
    #[derive(serde::Serialize)]
    struct Kept<'a> {
        store: &'a str,
        firsts: HashMap<i64, i64>,
    }
    let mut firsts = take_firsts(store, identity);
    firsts.extend(found);
    let kept = Kept {
        store: identity.unwrap_or(""),
        firsts,
    };
    if let Ok(bytes) = serde_json::to_vec(&kept) {
        crate::keep_beside(&firsts_file(store), &bytes);
    }
}

/// What the person did that [`person`] counts.
#[derive(Clone, Copy)]
enum Person {
    /// The messages they sent, from `messages`.
    Messages,
    /// The calls they decided, from `audit`.
    Calls,
}

/// Each `what` the person did for an agent in `scope` from `before` to
/// `now`, read in [`AT_ONCE`] slices of the span side by side, from
/// connection `first` on, each paged to its end: the daemon answers one
/// connection's reads in turn, and a page's cost is its rows. None when any
/// page could not be read.
async fn person(
    clients: &[Arc<agent_client::Client>],
    first: usize,
    what: Person,
    scope: &str,
    (before, now): (i64, i64),
) -> Option<Vec<Did>> {
    let step = ((now - before) / AT_ONCE as i64).max(1);
    let mut reads = tokio::task::JoinSet::new();
    let mut at = before;
    while at < now {
        let until = (at + step).min(now);
        let client = clients[(first + reads.len()) % clients.len()].clone();
        let scope = scope.to_owned();
        reads.spawn(async move { person_pages(&client, what, &scope, (at, until)).await });
        at = until;
    }
    let mut done = Vec::new();
    while let Some(read) = reads.join_next().await {
        done.extend(read.ok()??);
    }
    Some(done)
}

/// Each of `what` the person did for an agent in `scope` from `since` to
/// `until`, every page of it: the agent, its number, the message's and when.
async fn person_pages(
    client: &agent_client::Client,
    what: Person,
    scope: &str,
    (since, until): (i64, i64),
) -> Option<Vec<Did>> {
    let (op, list, time, next, mut query) = match what {
        Person::Messages => (
            "messages",
            "messages",
            "sent_ms",
            ("next_after", "after"),
            json!({"bot": scope, "from": ["person"], "limit": 256}),
        ),
        Person::Calls => (
            "audit",
            "rows",
            "at_ms",
            ("next_before", "before"),
            json!({"approved": "person", "bot": scope, "limit": 500}),
        ),
    };
    (query["since_ms"], query["until_ms"]) = (json!(since), json!(until));
    let mut done = Vec::new();
    loop {
        let page = client.request(op, query.clone()).await.ok()?;
        for row in page[list].as_array().into_iter().flatten() {
            if let (Some(bot), Some(bot_id), Some(at)) = (
                row["bot"].as_str(),
                row["bot_id"].as_i64(),
                row[time].as_i64(),
            ) {
                let message = row["message"].as_i64();
                done.push(Did {
                    bot: bot.to_owned(),
                    bot_id,
                    message,
                    at,
                });
            }
        }
        match &page[next.0] {
            Value::Null => return Some(done),
            more => query[next.1] = more.clone(),
        }
    }
}

/// Each of `tasks` that worked in the period from `start` to `end`, as
/// [`Tasks::tally`] takes it: its turns and their time from `spent`, the
/// person's messages in `wrote` and calls in `decided`. A task whose turn
/// started before `end` and still runs counts as running, its turn's time
/// so far with the rest, so one still at work when an earlier period ended
/// is not counted as ended there, and one whose last turn started before
/// the period and ended in it counts with that turn's time. Its time is
/// none when a turn it finished went untimed, which `usage` leaves out of
/// `turn_ms`.
fn rows(
    tasks: &[Listed],
    spent: &HashMap<String, Worked>,
    wrote: &HashMap<i64, u64>,
    decided: &HashMap<i64, u64>,
    (start, end): (i64, i64),
) -> Vec<Value> {
    let mut rows = Vec::new();
    for task in tasks {
        let spent = spent.get(&task.bot).copied().unwrap_or_default();
        // Running at the period's end: its turn runs now, or its last
        // turn started before the end and finished after it.
        let running = (task.running.filter(|started| *started < end)).or_else(|| {
            (task.ended)
                .filter(|(started, ended)| *started < end && *ended >= end)
                .map(|(started, _)| started)
        });
        // Its last turn, started before the period and ended in it, which
        // `usage` counts where it started.
        let crossed = task.ended.filter(|(started, ended)| {
            *started < start && (start..end).contains(ended) && running.is_none()
        });
        if spent.turns == 0 && running.is_none() && crossed.is_none() {
            continue;
        }
        // Its running turn, when it started in the period, is among its
        // turns but not yet timed.
        let open = running.is_some_and(|started| started >= start) as u64;
        let work_ms = (spent.turns == spent.timed + open).then(|| {
            let so_far = running.map_or(0, |started| (end - started).max(0) as u64);
            let crossed = crossed.map_or(0, |(started, ended)| (ended - started).max(0) as u64);
            spent.turn_ms + so_far + crossed
        });
        let count = |of: &HashMap<i64, u64>| of.get(&task.bot_id).copied().unwrap_or(0);
        rows.push(json!({
            "bot": task.bot, "turns": spent.turns, "work_ms": work_ms,
            "running": running.is_some(), "last_turn": task.last,
            "person_messages": count(wrote), "person_calls": count(decided),
            "created_by": task.created_by, "expire_ms": task.expire_ms,
            "profile": task.profile, "closed": task.closed,
        }));
    }
    rows
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

    fn made(bot: &str, work_min: u64, running: bool, wrote: u64, calls: u64) -> Value {
        json!({
            "bot": bot, "turns": 2, "work_ms": work_min * 60_000, "running": running,
            "last_turn": 9, "person_messages": wrote, "person_calls": calls,
            "created_by": "p.lead", "expire_ms": null,
        })
    }

    #[test]
    fn tasks_count_those_that_ended_without_the_person() {
        let made_rows = [
            made("p.fix-login", 20, false, 0, 0),
            made("p.add-export", 50, false, 1, 0),
            made("p.rename", 10, false, 0, 1),
            made("p.migrate", 200, false, 0, 2),
            made("p.tidy", 40, false, 0, 0),
            made("p.still", 5, true, 3, 0),
            // Leads, workflow workers and Sonny's own are no tasks.
            made("p.web.lead", 5, false, 0, 0),
            made("p.wf-1.wf-step", 5, false, 0, 0),
            made(".sonny-namer", 5, false, 0, 0),
        ];
        // A side chat of a task: made by it, expiring.
        let mut side = made("p.tidy.side-a1", 5, false, 1, 0);
        (side["created_by"], side["expire_ms"]) = (json!("p.tidy"), json!(3_600_000));
        // A task with an untimed turn: counted, but by no length.
        let mut untimed = made("p.quick", 0, false, 0, 0);
        untimed["work_ms"] = Value::Null;
        let mut rows = [&made_rows[..], &[side, untimed]].concat();
        // Each as the daemon keeps the profile it was made in.
        for row in &mut rows {
            if row["bot"] == "p.migrate" || row["bot"] == "p.tidy" {
                row["profile"] = json!("backend");
            }
        }
        // Closed, so done.
        for row in &mut rows {
            if row["bot"] == "p.fix-login" || row["bot"] == "p.migrate" {
                row["closed"] = json!(true);
            }
        }
        let tasks = Tasks::tally(&rows);
        assert_eq!((tasks.worked, tasks.working, tasks.ended), (7, 1, 6));
        assert_eq!(tasks.alone, 3);
        // The running task's message is not counted until it ends.
        assert_eq!((tasks.messages, tasks.calls), (1, 3));
        assert_eq!(tasks.done, 2);
        assert_eq!(tasks.lengths, [(1, 2), (1, 2), (0, 1)]);
        assert_eq!(
            tasks.roles,
            [("no role".to_owned(), 2, 4), ("backend".to_owned(), 1, 2)]
        );
        assert_eq!(Tasks::before(&rows), (3, 6));
    }

    #[test]
    fn each_period_counts_its_turns_and_what_the_person_did_in_them() {
        let listed = |bot: &str, bot_id: i64, running: Option<i64>| Listed {
            bot: bot.to_owned(),
            bot_id,
            running,
            last: Some(9),
            ended: None,
            created_by: json!("p.lead"),
            expire_ms: Value::Null,
            profile: Value::Null,
            closed: false,
        };
        let tasks = [
            listed("p.a", 1, None),
            listed("p.b", 2, Some(180)),
            listed("p.old", 3, None),
            listed("p.long", 4, Some(50)),
            listed("p.untimed", 5, None),
            // Its one turn started at 50, before this period, and ended in
            // it at 150.
            Listed {
                ended: Some((50, 150)),
                ..listed("p.crossed", 6, None)
            },
            // Its one turn started at 80, in the period before, and ended
            // at 150, in this one.
            Listed {
                ended: Some((80, 150)),
                ..listed("p.spanned", 8, None)
            },
        ];
        let spent = |turns, timed, turn_ms| Worked {
            turns,
            timed,
            turn_ms,
        };
        // The person's, by agent number: p.a's own number 1, and 7, an
        // agent of the same name before it, gone since, which is not p.a.
        let by_id = |pairs: &[(i64, u64)]| -> HashMap<i64, u64> { pairs.iter().copied().collect() };
        // p.old worked only the period before; p.b runs one turn, started
        // at 180; p.long's turn started at 50, before this period, and
        // runs still; one of p.untimed's two finished turns went untimed.
        let this = HashMap::from([
            ("p.a".to_owned(), spent(2, 2, 30)),
            ("p.b".to_owned(), spent(1, 0, 0)),
            ("p.untimed".to_owned(), spent(2, 1, 40)),
        ]);
        let last = HashMap::from([
            ("p.old".to_owned(), spent(1, 1, 10)),
            ("p.long".to_owned(), spent(1, 0, 0)),
            ("p.spanned".to_owned(), spent(1, 1, 70)),
        ]);
        let rows_now = rows(
            &tasks,
            &this,
            &by_id(&[(1, 1), (7, 4)]),
            &by_id(&[(2, 2)]),
            (100, 200),
        );
        let shown: Vec<_> = (rows_now.iter())
            .map(|row| {
                (
                    row["bot"].as_str().unwrap(),
                    row["turns"].as_u64().unwrap(),
                    row["work_ms"].as_u64(),
                    row["running"] == true,
                    row["person_messages"].as_u64().unwrap(),
                    row["person_calls"].as_u64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            shown,
            [
                ("p.a", 2, Some(30), false, 1, 0),
                // Running: its turn counts to now.
                ("p.b", 1, Some(20), true, 0, 2),
                // Running since before the period: still at work in it.
                ("p.long", 0, Some(150), true, 0, 0),
                // An untimed turn: no time to count it by.
                ("p.untimed", 2, None, false, 0, 0),
                // Ended in this period: counted with its turn's time.
                ("p.crossed", 0, Some(100), false, 0, 0),
                ("p.spanned", 0, Some(70), false, 0, 0),
            ]
        );
        let before = rows(&tasks, &last, &by_id(&[(3, 1)]), &HashMap::new(), (0, 100));
        let shown: Vec<_> = (before.iter())
            .map(|row| (row["bot"].as_str().unwrap(), row["running"] == true))
            .collect();
        // p.long had not ended when that period did, nor had p.crossed's
        // and p.spanned's turns; p.b's turn started after it.
        assert_eq!(
            shown,
            [
                ("p.old", false),
                ("p.long", true),
                ("p.crossed", true),
                ("p.spanned", true)
            ]
        );
        assert_eq!(before[0]["person_messages"], 1);
        assert_eq!(Tasks::before(&before), (0, 1));
    }

    #[test]
    fn tasks_show_with_no_turn_started_in_the_period() {
        let mut view = View::new(scope());
        let (_, window) = view.query(1_800_000_000_000);
        let tasks = Tasks {
            worked: 1,
            working: 1,
            ..Tasks::default()
        };
        view.loaded(window, Ok(json!({"turns": 0, "tasks": tasks})), Vec::new());
        let card = text(&view.card(80));
        assert!(card.contains("No turns in the "), "{card}");
        assert!(card.contains("Tasks  1 worked in the "), "{card}");
    }

    #[test]
    fn tasks_show_once_one_worked() {
        assert_eq!(Tasks::of(&Value::Null), None);
        assert_eq!(Tasks::of(&json!(Tasks::default())), None);
        let tasks = Tasks {
            worked: 2,
            working: 2,
            ..Tasks::default()
        };
        let lines = Tasks::of(&json!(tasks)).unwrap().lines(Period::Week, 80);
        // Nothing ended yet: the head alone.
        assert_eq!(
            text(&lines),
            "Tasks  2 worked in the last 7 days · 2 still working\n"
        );
    }

    #[test]
    fn no_task_row_is_wider_than_the_card() {
        let tasks = Tasks {
            worked: 1234,
            working: 34,
            ended: 1200,
            alone: 845,
            messages: 912,
            calls: 4021,
            done: 1001,
            lengths: [(500, 600), (300, 450), (45, 150)],
            roles: vec![
                (
                    "a-role-with-a-name-longer-than-the-column".to_owned(),
                    600,
                    800,
                ),
                ("frontend".to_owned(), 245, 400),
            ],
            before: Some((400, 1000)),
        };
        for width in 30..=120 {
            for line in tasks.lines(Period::Month, width) {
                assert!(
                    line.width() <= width,
                    "{} columns in {width}: {:?}",
                    line.width(),
                    text(&[line])
                );
            }
        }
        let wide = text(&tasks.lines(Period::Day, 100));
        assert!(wide.contains("Finished without you"), "{wide}");
        assert!(
            wide.contains(" 845 of 1200  70%  was 40% the day before"),
            "{wide}"
        );
        assert!(
            wide.contains("you wrote 912 · calls you decided 4021"),
            "{wide}"
        );
        assert!(wide.contains("Without you, by role"), "{wide}");
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
    fn each_agent_a_row_is_named_for_opens_it_but_this_windows_own() {
        let mut view = View::new(scope());
        let (_, window) = view.query(1_800_000_000_000);
        let bot = |bot: &str| {
            json!({"bot": bot, "turns": 1, "input_tokens": 1000,
            "cached_input_tokens": 500, "output_tokens": 100})
        };
        view.loaded(
            window,
            Ok(json!({
                "turns": 2, "input_tokens": 2000, "cached_input_tokens": 1000, "output_tokens": 200,
                "bots": [bot("p.fix"), bot("p.lead")],
                "other_bots": null, "models": [], "origins": [], "series": [],
            })),
            prices(""),
        );
        let (card, links) = view.card_linked(80, "p.lead");
        assert!(!links.is_empty());
        for (row, from, to, bot) in &links {
            assert_eq!(bot, "p.fix");
            let named: String = text(std::slice::from_ref(&card[*row]))
                .chars()
                .skip(usize::from(*from))
                .take(usize::from(to - from))
                .collect();
            assert_eq!(named, "fix");
            assert_eq!(card[*row].spans[0].style.fg, Some(Color::Blue));
        }
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
        view.key(KeyEvent::from(KeyCode::Right));
        view.key(KeyEvent::from(KeyCode::Right));
        let (query, window) = view.query(now);
        // Read by the hour whatever the period, the chart's bars days.
        assert_eq!(
            (query["bucket_ms"].as_i64(), window.1, window.2),
            (Some(HOUR), DAY, 30)
        );
        // Enter opens nothing here and never closes it; Esc does.
        assert_eq!(view.key(KeyEvent::from(KeyCode::Enter)), Key::Stay);
        assert_eq!(view.key(KeyEvent::from(KeyCode::Esc)), Key::Close);
    }

    /// A synthetic `usage` reply for the Daemon block: 9,214 turns in
    /// the period, those that ended with their calls and time, by the
    /// hour from `since`.
    fn daemon_usage(since: i64) -> Value {
        json!({
            "turns": 9214, "input_tokens": 4000, "cached_input_tokens": 3000, "output_tokens": 300,
            "bots": [], "other_bots": null, "models": [], "origins": [],
            "time": {
                "turns": 9180, "turn_ms": 1_000_000, "model_ms": 610_000, "tool_ms": 310_000,
                "paced_ms": 60_000, "summary_ms": 15_000, "daemon_ms": 2_000,
                "tool_calls": 148_300, "timed_rounds": 61_480,
            },
            "series": [
                {"at_ms": since, "turns": 40, "input_tokens": 10, "output_tokens": 1},
                {"at_ms": since + 5 * HOUR, "turns": 212, "input_tokens": 10, "output_tokens": 1},
                {"at_ms": since + 6 * HOUR, "turns": 90, "input_tokens": 10, "output_tokens": 1},
            ],
        })
    }

    /// A synthetic `stats` reply, up `uptime_s` seconds with `requests`
    /// answered.
    fn daemon_stats(uptime_s: u64, requests: u64) -> Value {
        json!({
            "active_turns": 27, "active_limit": 64, "paced_turns": 3,
            "running_processes": 14, "process_limit": 32,
            "commands": {"ops": {"usage": {"count": requests / 2}, "submit": {"count": requests - requests / 2}}},
            "store": {"jobs": 99_999, "groups": {"count": 10, "jobs": uptime_s * 6}},
            "hub": {"delivered": uptime_s * 190, "dropped": 0},
            "process": {
                "uptime_ms": uptime_s * 1000, "cpu_user_ms": uptime_s * 60, "cpu_system_ms": uptime_s * 30,
                "rss_bytes": 412u64 << 20, "peak_rss_bytes": 655u64 << 20,
            },
        })
    }

    fn block(card: &str) -> Vec<&str> {
        card.lines()
            .skip_while(|line| !line.starts_with("Daemon"))
            .collect()
    }

    #[test]
    fn the_daemon_block_says_what_it_did_and_where_a_turns_time_went() {
        let mut view = View::new(scope());
        // A week: read by the hour, the busiest hour the most turns any
        // hour had; the chart's bars sum six hours each.
        view.key(KeyEvent::from(KeyCode::Right));
        let (query, window) = view.query(1_800_000_000_000);
        assert_eq!(query["bucket_ms"], HOUR);
        view.loaded(window, Ok(daemon_usage(window.0)), Vec::new());
        let read = view.read.as_mut().unwrap();
        read.now = read.since + 168 * HOUR;
        let card = text(&view.card(90));
        assert_eq!(
            block(&card)[..13],
            [
                "Daemon  9,214 turns · 61,480 model calls · 148,300 tool calls",
                "",
                "What it did                     in the period     each hour    busiest hour",
                "  turns                                 9,214            55             212",
                "  model calls                          61,480           366",
                "  tool calls                          148,300           883",
                "  calls are counted in the 9,180 turns that ended",
                "",
                "Where a turn's time went",
                "  the model                     █████████████░░░░░░░░░   61%",
                "  tools                         ███████░░░░░░░░░░░░░░░   31%",
                "  waiting on a provider's limit █░░░░░░░░░░░░░░░░░░░░░    6%",
                "  summaries                     ░░░░░░░░░░░░░░░░░░░░░░    2%",
            ],
            "{card}"
        );
        // The daemon's own time under one percent; what none of the parts
        // records, its own row, nothing scaled to make 100.
        assert!(
            card.contains("  the daemon                    ░░░░░░░░░░░░░░░░░░░░░░   <1%"),
            "{card}"
        );
        assert!(
            card.contains("  the rest                      ░░░░░░░░░░░░░░░░░░░░░░   <1%"),
            "{card}"
        );
        // The chart's first bar sums its six hours, the first and the
        // sixth (22 tokens), and reaches the top; the next holds one (11).
        let lines: Vec<&str> = card.lines().collect();
        assert!(lines[4].starts_with("22 ┤██    "), "{card}");
        assert!(lines[8].starts_with("   │██ ██ "), "{card}");
        // Until `stats` lands, the card draws and its last table reads.
        assert!(
            card.ends_with("Right now  the whole daemon · reading…"),
            "{card}"
        );
    }

    #[test]
    fn a_period_with_no_timed_turns_says_nothing_of_calls_or_time() {
        let mut view = View::new(scope());
        let (_, window) = view.query(1_800_000_000_000);
        let mut usage = daemon_usage(window.0);
        usage["time"] = json!({"turns": 0, "turn_ms": 0, "tool_calls": 0, "timed_rounds": 0});
        view.loaded(window, Ok(usage), Vec::new());
        let card = text(&view.card(90));
        assert_eq!(block(&card)[0], "Daemon  9,214 turns", "{card}");
        assert!(!card.contains("model calls"), "{card}");
        assert!(!card.contains("Where a turn's time went"), "{card}");
        // No turns at all: the state now, nothing of the period.
        let mut view = View::new(scope());
        view.loaded(
            window,
            Ok(json!({"turns": 0, "bots": [], "models": []})),
            Vec::new(),
        );
        view.daemon.lock().unwrap().came(Ok(daemon_stats(100, 50)));
        let card = text(&view.card(90));
        assert!(card.contains("No turns in the last 24 hours."), "{card}");
        assert_eq!(
            block(&card)[..3],
            ["Daemon  0 turns", "", "Right now  the whole daemon · up 1m"],
            "{card}"
        );
    }

    #[test]
    fn right_now_reads_a_rate_between_two_reads_and_starts_over_when_the_daemon_does() {
        let mut readings = Readings::default();
        // One read: each rate since it started, and only that.
        readings.came(Ok(daemon_stats(187_200, 3_931_200)));
        assert_eq!(
            text(&readings.lines(90)).lines().collect::<Vec<_>>(),
            [
                "Right now  the whole daemon · up 2d 4h",
                "  requests                       21 a second   since it started",
                "  store writes                    6 a second   since it started",
                "  events sent                   190 a second   since it started",
                "  turns running                     27 of 64   3 more wait on a provider's limit",
                "  commands running                  14 of 32",
                "  processor                   9% of one core   since it started",
                "  memory                              412 MB   most 655 MB",
            ]
        );
        // A minute on: what it counted between the two over the time
        // between them, by its own clock.
        let mut later = daemon_stats(187_260, 3_931_200 + 60 * 38);
        later["hub"]["delivered"] = json!(187_200 * 190 + 60 * 410);
        later["process"]["cpu_system_ms"] = json!(187_200 * 30 + 60 * 120);
        later["process"]["cpu_user_ms"] = json!(187_200 * 60);
        readings.came(Ok(later));
        let lines = text(&readings.lines(90));
        assert!(
            lines.contains(
                "  requests                       38 a second   21 a second since it started"
            ),
            "{lines}"
        );
        assert!(
            lines.contains(
                "  events sent                   410 a second   190 a second since it started"
            ),
            "{lines}"
        );
        assert!(
            lines.contains("  processor                  12% of one core   9% since it started"),
            "{lines}"
        );
        // Started again since: no rate across it, until a second read.
        readings.came(Ok(daemon_stats(30, 300)));
        let lines = text(&readings.lines(90));
        assert!(lines.contains("· up 30s"), "{lines}");
        assert!(
            lines.contains("  requests                       10 a second   since it started"),
            "{lines}"
        );
        // A count that fell is a start too, however long it says it ran.
        readings.came(Ok(daemon_stats(31, 100)));
        assert!(text(&readings.lines(90)).contains("3.2 a second   since it started"));
        // What the daemon reports as null leaves its row or cell out, and
        // a limit of none (0) says no `of`.
        let mut bare = daemon_stats(100, 100);
        bare["process"]["rss_bytes"] = Value::Null;
        bare["process"]["cpu_user_ms"] = Value::Null;
        bare["process_limit"] = json!(0);
        bare["paced_turns"] = json!(0);
        bare["hub"] = Value::Null;
        let mut readings = Readings::default();
        readings.came(Ok(bare));
        let lines = text(&readings.lines(90));
        assert!(!lines.contains("memory"), "{lines}");
        assert!(!lines.contains("processor"), "{lines}");
        assert!(!lines.contains("events sent"), "{lines}");
        assert!(
            lines.contains("  turns running                     27 of 64\n"),
            "{lines}"
        );
        assert!(
            lines.contains("  commands running                        14"),
            "{lines}"
        );
        // A read that failed says so, and the next one fills it in.
        let mut readings = Readings::default();
        readings.came(Err("connection refused".into()));
        assert_eq!(
            text(&readings.lines(90)),
            "Right now  the whole daemon · could not read it: connection refused"
        );
    }

    #[test]
    fn one_stats_read_at_a_time() {
        let view = View::new(scope());
        let kept = view.stats_due().expect("the first read starts");
        assert!(view.stats_due().is_none());
        kept.lock().unwrap().came(Ok(daemon_stats(10, 10)));
        assert!(view.stats_due().is_some());
    }

    #[test]
    fn no_row_of_the_daemon_block_is_wider_than_the_card() {
        let mut view = View::new(scope());
        let (_, window) = view.query(1_800_000_000_000);
        view.loaded(window, Ok(daemon_usage(window.0)), Vec::new());
        let mut readings = view.daemon.lock().unwrap();
        readings.came(Ok(daemon_stats(187_200, 3_931_200)));
        readings.came(Ok(daemon_stats(187_260, 3_933_480)));
        drop(readings);
        // Narrower than it is laid out for, a name gives way, then a column
        // or a note goes whole: nothing in it is cut short.
        for width in [64, 72, 80] {
            let card = text(&view.card(width));
            assert!(!block(&card).iter().any(|line| line.contains('…')), "{card}");
            assert!(card.contains("busiest hour") == (width >= 58), "{card}");
        }
        for width in 20..=100 {
            let card = view.card(width);
            for line in block(&text(&card)) {
                assert!(
                    line.width() <= width,
                    "{} columns in {width}: {line:?}",
                    line.width()
                );
            }
        }
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
    #[tokio::test]
    async fn a_tasks_first_message_is_read_once_and_kept_beside_the_store() {
        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = asked.clone();
        // `a.one` began with the person's 10; `b.two` with its lead's 15.
        let client = crate::tests::daemon_answering(move |request| {
            seen.lock()
                .unwrap()
                .push(request["bot"].as_str().unwrap_or_default().to_owned());
            match request["bot"].as_str() {
                Some("b.two") => json!({"messages": [{"message": 15}]}),
                _ => json!({"messages": []}),
            }
        })
        .await;
        let did = |bot: &str, bot_id, message| Did {
            bot: bot.to_owned(),
            bot_id,
            message: Some(message),
            at: message,
        };
        let wrote = || {
            vec![
                did("a.one", 1, 10),
                did("a.one", 1, 12),
                did("b.two", 2, 20),
            ]
        };
        let left = |wrote: &[Did]| {
            wrote
                .iter()
                .map(|did| did.message.unwrap())
                .collect::<Vec<_>>()
        };
        let (first, found) =
            without_briefs(std::slice::from_ref(&client), wrote(), &HashMap::new())
                .await
                .unwrap();
        assert_eq!(left(&first), [12, 20]);
        assert_eq!(found, HashMap::from([(1, 10), (2, 15)]));
        let mut bots = std::mem::take(&mut *asked.lock().unwrap());
        bots.sort();
        assert_eq!(bots, ["a.one", "b.two"]);
        // Kept beside the store for the daemon that numbered them, another
        // window's read is the same with no read at all.
        let dir = std::env::temp_dir().join(format!("sonny-firsts-{}", std::process::id()));
        let store = dir.join("state.sqlite");
        keep_firsts(&store, Some("store-1"), HashMap::from([(1, 10)]));
        keep_firsts(&store, Some("store-1"), HashMap::from([(2, 15)]));
        let kept = take_firsts(&store, Some("store-1"));
        assert_eq!(kept, found);
        assert!(take_firsts(&store, Some("store-2")).is_empty());
        let (again, found) = without_briefs(&[client], wrote(), &kept).await.unwrap();
        assert_eq!(left(&again), [12, 20]);
        assert!(found.is_empty());
        assert!(asked.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
