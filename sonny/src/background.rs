//! `/background`: what runs for the project's agents with no one typing,
//! read from files with no model or daemon call: the triggers that wake
//! them, which launchd keeps (`trigger`), and the lookout's subscriptions
//! for them (`lookout::subs`). From there a trigger is fired now or
//! removed, and a subscription removed; the one Sonny writes for a lead
//! stays, since its project opening writes it again. How many an agent
//! has is the `↻` its footer and its `/status` row show.
use crate::{lookout, trigger};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    style::{Color, Stylize},
    text::{Line, Span},
};
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Trigger,
    Subscription,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub kind: Kind,
    pub name: String,
    /// Whom it is for: the agent a trigger messages, or the subscriber.
    pub bot: String,
    /// Every agent it is for: a trigger's also the one that added it to
    /// start its agent, and the one its answers go to.
    pub bots: Vec<String>,
    /// When it acts: a trigger's time or event.
    pub when: String,
    /// What it does: a trigger's message, a subscription's watch.
    pub what: String,
    /// What came of it: a trigger's sends and last fire, a subscription's
    /// checks in the last day.
    pub facts: Vec<String>,
    /// Who gets a trigger's answers, when not the agent it messages.
    pub reply_to: Option<String>,
    /// Sonny's own: a lead's `tasks` subscription. Never counted, never
    /// removed.
    pub own: bool,
}

/// Every trigger made on the daemon whose store is `store_id` and every
/// subscription beside `store` that is for an agent `ours` takes.
pub fn read(store: &Path, store_id: Option<&str>, ours: impl Fn(&str) -> bool) -> Vec<Item> {
    let mut items = Vec::new();
    if let (Some(id), Ok(places)) = (store_id, trigger::Places::home()) {
        for (row, bots) in trigger::on_store(&places, id) {
            if bots.iter().any(|bot| ours(bot)) {
                items.push(trigger_item(&row, bots));
            }
        }
    }
    for (subscriber, subs) in lookout::subs::all(store) {
        if !ours(&subscriber) {
            continue;
        }
        let project = subscriber.strip_suffix(".lead");
        for sub in subs {
            let own = project.is_some_and(|p| sub == lookout::subs::Sub::lead(&subscriber, p));
            let described = sub.describe();
            let what = described
                .strip_prefix(&format!("{} · ", sub.name))
                .unwrap_or(&described)
                .to_owned();
            items.push(Item {
                kind: Kind::Subscription,
                name: sub.name,
                bot: subscriber.clone(),
                bots: vec![subscriber.clone()],
                when: String::new(),
                what,
                facts: Vec::new(),
                reply_to: None,
                own,
            });
        }
    }
    items
}

fn trigger_item(row: &Value, bots: Vec<String>) -> Item {
    let text = |key: &str| row[key].as_str().unwrap_or_default().to_owned();
    let mut facts = Vec::new();
    let sent = row["sent"].as_u64().unwrap_or(0);
    let last = &row["last"];
    facts.push(match row["runs"].as_u64() {
        Some(runs) => format!("sent {sent} of {runs}"),
        None if sent == 0 && last.is_null() => "not fired yet".into(),
        None => format!("sent {sent}"),
    });
    if let Some(outcome) = last["outcome"].as_str() {
        let ago = last["fired_ms"]
            .as_u64()
            .map(|at| format!(" {}", ago(now_ms().saturating_sub(at) / 1000)))
            .unwrap_or_default();
        let detail = match last["detail"].as_str() {
            Some(detail) if outcome != "sent" => format!(": {}", first_line(detail)),
            _ => String::new(),
        };
        facts.push(format!("last {outcome}{ago}{detail}"));
    }
    if row["missed"] == true {
        facts.push("missed its time".into());
    }
    if let Some(gate) = row["if"].as_str() {
        facts.push(format!("if {}", first_line(gate)));
    }
    Item {
        kind: Kind::Trigger,
        name: text("name"),
        bot: text("bot"),
        bots,
        when: text("when"),
        what: first_line(&text("message")),
        facts,
        reply_to: row["reply_to"].as_str().map(str::to_owned),
        own: false,
    }
}

/// How many of `items` are for `bot`, Sonny's own left out.
pub fn count(items: &[Item], bot: &str) -> usize {
    items
        .iter()
        .filter(|item| !item.own && item.bots.iter().any(|b| b == bot))
        .count()
}

/// What `/status` says of `bot`'s: `↻ 2 triggers, 1 subscription`, or
/// nothing.
pub fn fact(items: &[Item], bot: &str) -> String {
    let of = |kind: Kind| {
        items
            .iter()
            .filter(|i| i.kind == kind && !i.own && i.bots.iter().any(|b| b == bot))
            .count()
    };
    let said: Vec<String> = [
        (of(Kind::Trigger), "trigger"),
        (of(Kind::Subscription), "subscription"),
    ]
    .into_iter()
    .filter(|(n, _)| *n > 0)
    .map(|(n, what)| format!("{n} {what}{}", if n == 1 { "" } else { "s" }))
    .collect();
    match said.is_empty() {
        true => String::new(),
        false => format!("↻ {}", said.join(", ")),
    }
}

/// The agents a window's `/background` covers: the project's, as
/// `/status` lists them, by a glob.
pub struct Scope {
    pub project: String,
    pub lead: String,
    pub glob: String,
}

impl Scope {
    fn takes(&self, bot: &str) -> bool {
        lookout::subs::matches(&self.glob, bot)
    }
    /// A bot's name in the project: `lead`, or the rest after `project.`.
    pub fn short(&self, bot: &str) -> String {
        if bot == self.lead {
            return "lead".into();
        }
        bot.strip_prefix(&format!("{}.", self.project))
            .unwrap_or(bot)
            .to_owned()
    }
}

/// `/background`, open: what it lists, the row picked, and a removal
/// waiting for its second `d`.
pub struct View {
    scope: Scope,
    store: PathBuf,
    store_id: Option<String>,
    /// The window's own agent, whose lookout line heads the card.
    bot: String,
    items: Vec<Item>,
    lookout: String,
    selected: usize,
    /// The item the first `d` picked, by kind, agent and name.
    confirm: Option<(Kind, String, String)>,
    notice: Option<String>,
}

/// What a key did to the view.
#[derive(Debug, PartialEq)]
pub enum Key {
    Stay,
    Close,
}

impl View {
    pub fn open(scope: Scope, store: PathBuf, store_id: Option<String>, bot: String) -> Self {
        let mut view = Self {
            scope,
            store,
            store_id,
            bot,
            items: Vec::new(),
            lookout: String::new(),
            selected: 0,
            confirm: None,
            notice: None,
        };
        view.reload();
        view
    }

    /// Read it all again, the pick kept on the same row where it still is.
    pub fn reload(&mut self) {
        let picked = self.items.get(self.selected).map(key_of);
        let items = read(&self.store, self.store_id.as_deref(), |b| {
            self.scope.takes(b)
        });
        let mut items = self.with_checks(items);
        items.sort_by_key(|item| item.kind == Kind::Subscription);
        self.items = items;
        self.selected = picked
            .and_then(|k| self.items.iter().position(|i| key_of(i) == k))
            .unwrap_or(self.selected)
            .min(self.items.len().saturating_sub(1));
        // A removal waits only on the row it was asked for, still picked.
        if self.confirm.is_some() && self.items.get(self.selected).map(key_of) != self.confirm {
            self.confirm = None;
        }
        self.lookout = lookout::status(&self.store, &self.bot);
    }

    fn with_checks(&self, mut items: Vec<Item>) -> Vec<Item> {
        let mut seen = std::collections::HashMap::new();
        for item in items.iter_mut().filter(|i| i.kind == Kind::Subscription) {
            let checks = seen
                .entry(item.bot.clone())
                .or_insert_with(|| lookout::checks(&self.store, &item.bot));
            let n = checks.get(&item.name).copied().unwrap_or(0);
            item.facts.push(format!(
                "{n} check{} in the last day",
                if n == 1 { "" } else { "s" }
            ));
            if item.own {
                item.facts.push("Sonny's own, for the lead's tasks".into());
            }
        }
        items
    }

    /// The item a removal was asked for is kept: a key went elsewhere.
    pub fn keep(&mut self) {
        self.confirm = None;
    }

    pub fn key(&mut self, key: KeyEvent) -> Key {
        let confirm = self.confirm.take();
        self.notice = None;
        match key.code {
            KeyCode::Esc if confirm.is_some() => {}
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => return Key::Close,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Key::Close;
            }
            // A letter with Ctrl or Alt is another shortcut, such as Ctrl+F's
            // find: never a fire or a removal.
            KeyCode::Char(_)
                if key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) => {}
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(self.items.len().saturating_sub(1));
            }
            KeyCode::Char('f') => self.fire(),
            KeyCode::Char('d') | KeyCode::Delete => {
                let Some(item) = self.items.get(self.selected).cloned() else {
                    return Key::Stay;
                };
                if item.own {
                    self.notice = Some(
                        "Sonny keeps the lead's tasks subscription; the lookout watches its tasks with it"
                            .into(),
                    );
                } else if confirm == Some(key_of(&item)) {
                    self.remove(&item);
                } else {
                    self.confirm = Some(key_of(&item));
                }
            }
            _ => {}
        }
        Key::Stay
    }

    fn fire(&mut self) {
        let Some(item) = self.items.get(self.selected) else {
            return;
        };
        if item.kind != Kind::Trigger {
            self.notice = Some("only a trigger fires".into());
            return;
        }
        let name = item.name.clone();
        self.notice = Some(
            match trigger::Places::home().and_then(|places| trigger::fire_now(&places, &name)) {
                Ok(_) => format!("fired {name}"),
                Err(error) => error,
            },
        );
        self.reload();
    }

    fn remove(&mut self, item: &Item) {
        let done = match item.kind {
            Kind::Trigger => trigger::Places::home()
                .and_then(|places| trigger::remove(&places, &item.name, &trigger::launchctl)),
            Kind::Subscription => {
                lookout::subs::remove(&self.store, &item.bot, &item.name).map(drop)
            }
        };
        self.notice = Some(match done {
            Ok(()) => format!("removed {}", item.name),
            Err(error) => error,
        });
        self.reload();
    }

    /// The card, and the row the picked item starts on.
    pub fn card(&self, width: usize) -> (Vec<Line<'static>>, usize) {
        let mut out =
            vec![Line::from(format!("{} · in the background", self.scope.project)).bold()];
        if !self.lookout.is_empty() {
            out.push(Line::from(self.lookout.clone()).dim());
        }
        let mut at = 0;
        if self.items.is_empty() {
            out.push(Line::default());
            out.push(Line::from(
                "Nothing runs in the background for these agents.",
            ));
            out.push(
                Line::from(
                    "An agent adds a trigger with its triggers tool, and a lookout subscription with its subscriptions tool.",
                )
                .dim(),
            );
            return (out, 0);
        }
        for (kind, heading) in [
            (Kind::Trigger, "Triggers"),
            (Kind::Subscription, "Lookout subscriptions"),
        ] {
            let n = self.items.iter().filter(|i| i.kind == kind).count();
            if n == 0 {
                continue;
            }
            out.push(Line::default());
            out.push(Line::from(vec![
                Span::raw(heading).bold(),
                Span::raw(format!(" · {n}")).dim(),
            ]));
            for (index, item) in self.items.iter().enumerate() {
                if item.kind != kind {
                    continue;
                }
                if index == self.selected {
                    at = out.len();
                }
                out.extend(self.item_lines(item, index == self.selected, width));
            }
        }
        (out, at)
    }

    fn item_lines(&self, item: &Item, picked: bool, width: usize) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        let pick = match picked {
            true => Span::raw("›").fg(Color::Cyan).bold(),
            false => Span::raw(" "),
        };
        let mark = match item.kind {
            Kind::Trigger => Span::raw("↻ ").fg(Color::Cyan),
            Kind::Subscription => Span::raw("◎ ").fg(Color::Magenta),
        };
        let name = match picked {
            true => Span::raw(item.name.clone()).bold().reversed(),
            false => Span::raw(item.name.clone()).bold(),
        };
        let mut spans = vec![pick, mark, name];
        match item.kind {
            Kind::Trigger => {
                spans.push(Span::raw(format!(" {}", item.when)));
                spans.push(Span::raw(format!(" → {}", self.scope.short(&item.bot))).dim());
            }
            Kind::Subscription => {
                spans.push(Span::raw(format!(" for {}", self.scope.short(&item.bot))).dim());
            }
        }
        out.extend(crate::transcript::wrap(
            Line::from(spans),
            width,
            Span::raw(""),
            Span::raw("   "),
        ));
        let mut facts = item.facts.clone();
        if let Some(to) = item.reply_to.as_ref().filter(|to| **to != item.bot) {
            facts.push(format!("answers go to {}", self.scope.short(to)));
        }
        if !facts.is_empty() {
            out.extend(crate::transcript::wrap(
                Line::from(facts.join(" · ")).dim(),
                width,
                Span::raw("   "),
                Span::raw("   "),
            ));
        }
        if !item.what.is_empty() {
            out.extend(crate::transcript::capped(
                Line::from(item.what.clone()).dim(),
                width,
                Span::raw("   └ ").dim(),
                1,
            ));
        }
        out
    }

    /// The line under the card: what was done, the removal to confirm, or
    /// the keys the picked row takes.
    pub fn footer(&self) -> Line<'static> {
        if let Some(notice) = &self.notice {
            return Line::from(format!("  {notice}")).fg(Color::Yellow);
        }
        if let Some((_, _, name)) = &self.confirm {
            return Line::from(vec![
                Span::raw("  d again removes ").fg(Color::Yellow),
                Span::raw(name.clone()).bold().fg(Color::Yellow),
                Span::raw(" · any other key keeps it").dim(),
            ]);
        }
        let mut keys = Vec::new();
        if self.items.len() > 1 {
            keys.push("↑↓ pick");
        }
        match self.items.get(self.selected) {
            Some(item) if item.kind == Kind::Trigger => keys.extend(["f fires now", "d removes"]),
            Some(item) if !item.own => keys.push("d removes"),
            _ => {}
        }
        keys.push("esc to close");
        Line::from(format!("  {}", keys.join(" · "))).dim()
    }
}

fn key_of(item: &Item) -> (Kind, String, String) {
    (item.kind, item.bot.clone(), item.name.clone())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// How long ago, roughly: `40s ago`, `5m ago`, `3h ago`, `2d ago`.
fn ago(seconds: u64) -> String {
    match seconds {
        0..60 => format!("{seconds}s ago"),
        60..3600 => format!("{}m ago", seconds / 60),
        3600..86400 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86400),
    }
}

fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or("");
    let mut cut: String = line.chars().take(80).collect();
    if cut.len() < line.len() || text.lines().nth(1).is_some() {
        cut.push('…');
    }
    cut
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(kind: Kind, name: &str, bots: &[&str], own: bool) -> Item {
        Item {
            kind,
            name: name.into(),
            bot: bots[0].into(),
            bots: bots.iter().map(|b| b.to_string()).collect(),
            when: "every 1d".into(),
            what: "Read preferences.md".into(),
            facts: vec!["sent 2".into()],
            reply_to: None,
            own,
        }
    }

    #[test]
    fn a_bots_count_leaves_out_sonnys_own() {
        let items = [
            item(
                Kind::Trigger,
                "digest",
                &["calc.digest", "calc.lead"],
                false,
            ),
            item(Kind::Trigger, "nightly", &["calc.lead"], false),
            item(Kind::Subscription, "tasks", &["calc.lead"], true),
            item(Kind::Subscription, "prs", &["calc.lead"], false),
        ];
        assert_eq!(count(&items, "calc.lead"), 3);
        assert_eq!(count(&items, "calc.digest"), 1);
        assert_eq!(count(&items, "calc.fix"), 0);
        assert_eq!(fact(&items, "calc.lead"), "↻ 2 triggers, 1 subscription");
        assert_eq!(fact(&items, "calc.digest"), "↻ 1 trigger");
        assert_eq!(fact(&items, "calc.fix"), "");
    }

    #[test]
    fn a_trigger_says_what_its_fires_did() {
        let fired = now_ms() - 3 * 3600 * 1000;
        let row = json!({"name": "nightly", "when": "cron 0 8 * * *", "bot": "calc.lead",
            "message": "Read the inbox\nthen post", "sent": 4, "runs": 10,
            "reply_to": "calc.lead", "if": null,
            "last": {"outcome": "failed", "fired_ms": fired, "detail": "bot_busy: it works"}});
        let item = trigger_item(&row, vec!["calc.lead".into()]);
        assert_eq!(item.what, "Read the inbox…");
        assert_eq!(
            item.facts,
            ["sent 4 of 10", "last failed 3h ago: bot_busy: it works"]
        );
        assert_eq!(item.reply_to.as_deref(), Some("calc.lead"));
        let fresh = trigger_item(
            &json!({"name": "new", "when": "fire", "bot": "calc.lead", "sent": 0, "last": null}),
            vec!["calc.lead".into()],
        );
        assert_eq!(fresh.facts, ["not fired yet"]);
    }

    #[test]
    fn the_card_lists_triggers_then_subscriptions_and_d_asks_twice() {
        let dir = std::env::temp_dir().join(format!("background-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = dir.join("state.sqlite");
        let mut view = View {
            scope: Scope {
                project: "calc".into(),
                lead: "calc.lead".into(),
                glob: "calc.*".into(),
            },
            store,
            store_id: None,
            bot: "calc.lead".into(),
            items: vec![
                item(Kind::Trigger, "nightly", &["calc.lead"], false),
                item(Kind::Subscription, "tasks", &["calc.lead"], true),
            ],
            lookout: "Lookout · not running".into(),
            selected: 0,
            confirm: None,
            notice: None,
        };
        let text = |view: &View| -> Vec<String> {
            view.card(100)
                .0
                .iter()
                .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
                .collect()
        };
        let lines = text(&view);
        assert_eq!(lines[0], "calc · in the background");
        assert_eq!(lines[3], "Triggers · 1");
        assert_eq!(lines[4], "›↻ nightly every 1d → lead");
        assert_eq!(lines[5], "   sent 2");
        assert_eq!(lines[6], "   └ Read preferences.md");
        assert_eq!(lines[8], "Lookout subscriptions · 1");
        assert_eq!(lines[9], " ◎ tasks for lead");
        let press = |view: &mut View, code| view.key(KeyEvent::new(code, KeyModifiers::NONE));
        // Ctrl+D is not d.
        view.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert_eq!(view.confirm, None);
        assert_eq!(press(&mut view, KeyCode::Char('d')), Key::Stay);
        assert_eq!(
            view.confirm,
            Some((Kind::Trigger, "calc.lead".into(), "nightly".into()))
        );
        // A key the pane took instead keeps it.
        view.keep();
        assert_eq!(view.confirm, None);
        assert_eq!(press(&mut view, KeyCode::Char('d')), Key::Stay);
        assert!(view.confirm.is_some());
        // A reload that no longer finds it there forgets it.
        view.reload();
        assert_eq!(view.confirm, None);
        view.items = vec![
            item(Kind::Trigger, "nightly", &["calc.lead"], false),
            item(Kind::Subscription, "tasks", &["calc.lead"], true),
        ];
        press(&mut view, KeyCode::Char('d'));
        // Any other key keeps it; esc then only cancels.
        assert_eq!(press(&mut view, KeyCode::Esc), Key::Stay);
        assert_eq!(view.confirm, None);
        press(&mut view, KeyCode::Down);
        assert_eq!(view.card(100).1, 9);
        press(&mut view, KeyCode::Char('d'));
        assert_eq!(view.confirm, None);
        assert!(view.notice.as_deref().unwrap().starts_with("Sonny keeps"));
        assert_eq!(press(&mut view, KeyCode::Esc), Key::Close);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
