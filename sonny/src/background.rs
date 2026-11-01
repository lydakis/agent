//! `/background`: what runs for the project's agents with no one typing,
//! read from files with no model or daemon call: the triggers that wake
//! them, which the system's scheduler keeps (`trigger`), and the lookout's subscriptions
//! for them (`lookout::subs`). From there a trigger is fired now or
//! removed, and a subscription removed; the one Sonny writes for a lead
//! stays, since its project opening writes it again. How many an agent
//! has is the `↻` its footer and its `/status` row show. What failed comes
//! first, and is the `✗` beside it: a trigger whose fire failed, or whose
//! fire went out and whose turn then failed, as when the machine was
//! offline or a login expired, the daemon saying how the turn ended; and a
//! server the nightly memory cleanup could not read, by the fact it saves.
use crate::{lookout, trigger};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    style::{Color, Stylize},
    text::{Line, Span},
};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Trigger,
    Subscription,
    /// A server the nightly memory cleanup could not read: its
    /// `unreadable-SERVER` fact.
    Server,
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
    /// What came of it: a trigger's sends and last fire; whether a
    /// subscription is Sonny's own.
    pub facts: Vec<String>,
    /// Who gets a trigger's answers, when not the agent it messages.
    pub reply_to: Option<String>,
    /// Sonny's own: a lead's `tasks` subscription. Never counted, never
    /// removed.
    pub own: bool,
    /// Why it failed, when its last fire or the turn it sent did, or the
    /// server could not be read.
    pub failed: Option<String>,
    /// The turn a trigger's last fire sent, whose end `turns` tells.
    pub handle: Option<String>,
    /// When a trigger last fired, in ms: a fire that sent no turn is still
    /// a fire of its own.
    pub fired_ms: Option<u64>,
}

/// Every trigger made on the daemon whose store is `store_id` and every
/// subscription beside `store` that is for an agent `ours` takes, each
/// trigger's last fire with how its turn ended where `turns` has it.
pub fn read(
    store: &Path,
    store_id: Option<&str>,
    turns: &HashMap<String, Value>,
    ours: impl Fn(&str) -> bool,
) -> Vec<Item> {
    let mut items = Vec::new();
    if let (Some(id), Ok(places)) = (store_id, trigger::Places::home()) {
        for (mut row, bots) in trigger::on_store(&places, id) {
            if bots.iter().any(|bot| ours(bot)) {
                trigger::with_turn(&mut row, turns);
                items.push(trigger_item(&row, bots));
            }
        }
    }
    for (subscriber, subs) in lookout::subs::all(store) {
        if !ours(&subscriber) {
            continue;
        }
        let project = crate::project::lead_project(&subscriber);
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
                facts: match own {
                    true => vec!["Sonny's own, for the lead's tasks".into()],
                    false => Vec::new(),
                },
                reply_to: None,
                own,
                failed: None,
                handle: None,
                fired_ms: None,
            });
        }
    }
    items
}

/// The person's own agents, which no project names, as the one the nightly
/// memory cleanup starts: what fails for them is the person's to know in
/// every window.
pub fn personal(bot: &str) -> bool {
    !bot.contains('.')
}

/// Whether a window of `scope` shows `item`: one of its agents is the
/// scope's, or it failed and is the person's own.
pub fn shown(scope: &Scope, item: &Item) -> bool {
    item.bots.iter().any(|bot| scope.takes(bot))
        || (item.failed.is_some() && item.bots.iter().all(|bot| personal(bot)))
}

/// What a window of `scope` lists: its triggers and subscriptions, the
/// person's own that failed, and the servers the nightly memory cleanup
/// could not read, for the person and the project.
pub fn read_scope(
    store: &Path,
    store_id: Option<&str>,
    turns: &HashMap<String, Value>,
    scope: &Scope,
) -> Vec<Item> {
    let mut items = read_around(store, store_id, turns, scope);
    items.retain(|item| shown(scope, item));
    items
}

/// [`read_scope`] before what it does not show is left out: the person's
/// own triggers too, whose turns must be asked about before it is known
/// whether they failed.
pub fn read_around(
    store: &Path,
    store_id: Option<&str>,
    turns: &HashMap<String, Value>,
    scope: &Scope,
) -> Vec<Item> {
    let mut items = read(store, store_id, turns, |bot| {
        scope.takes(bot) || personal(bot)
    });
    if let Some(home) = std::env::var_os("HOME") {
        let root = PathBuf::from(home).join(".agents/memory");
        items.extend(servers(&root, &scope.project));
    }
    items
}

/// The `unreadable-SERVER` facts the nightly memory cleanup saves, the
/// person's and `project`'s, each as the server it names.
fn servers(root: &Path, project: &str) -> Vec<Item> {
    let view = agent_client::memory::view(root, Some(project));
    let scopes =
        std::iter::once(&view["user"]).chain(view["projects"].as_array().into_iter().flatten());
    let mut items = Vec::new();
    for scope in scopes {
        for fact in scope["facts"].as_array().into_iter().flatten() {
            let name = fact["name"].as_str().unwrap_or_default();
            let Some(server) = name.strip_prefix("unreadable-").filter(|s| !s.is_empty()) else {
                continue;
            };
            let why = fact["description"].as_str().unwrap_or_default();
            let mut facts = vec![format!("memory fact {name}")];
            if let Some(project) = scope["name"].as_str() {
                facts[0].push_str(&format!(" in {project}"));
            }
            if let Some(at) = fact["modified"].as_u64().filter(|at| *at > 0) {
                facts.push(format!("saved {}", ago(now_ms().saturating_sub(at) / 1000)));
            }
            facts.push("read again each night".into());
            items.push(Item {
                kind: Kind::Server,
                name: server.to_owned(),
                bot: String::new(),
                bots: Vec::new(),
                when: String::new(),
                what: String::new(),
                facts,
                reply_to: None,
                own: false,
                failed: Some(match why.is_empty() {
                    true => "could not be read".into(),
                    false => first_line(why),
                }),
                handle: None,
                fired_ms: None,
            });
        }
    }
    items
}

/// Each failure among `items`, as a key that stays the same while the
/// failure does: what failed, the fire that did (when it fired, the turn
/// it sent), and why. A later fire that fails again is a failure of its
/// own, also one that never sent a turn.
pub fn failures(items: &[Item]) -> Vec<String> {
    (items.iter())
        .filter_map(|item| {
            let why = item.failed.as_ref()?;
            let turn = item.handle.as_deref().unwrap_or("");
            let at = item.fired_ms.unwrap_or(0);
            Some(format!("{:?}/{}/{at}/{turn}/{why}", item.kind, item.name))
        })
        .collect()
}

/// The turns `items`' last fires sent whose end `turns` does not have yet.
pub fn unended(items: &[Item], turns: &HashMap<String, Value>) -> Vec<String> {
    (items.iter())
        .filter_map(|item| item.handle.clone())
        .filter(|handle| !turns.contains_key(handle))
        .collect()
}

fn trigger_item(row: &Value, bots: Vec<String>) -> Item {
    let text = |key: &str| row[key].as_str().unwrap_or_default().to_owned();
    let mut facts = Vec::new();
    let sent = row["sent"].as_u64().unwrap_or(0);
    let last = &row["last"];
    let mut failed = None;
    facts.push(match row["runs"].as_u64() {
        Some(runs) => format!("sent {sent} of {runs}"),
        None if sent == 0 && last.is_null() => "not run yet".into(),
        None => format!("sent {sent}"),
    });
    if let Some(outcome) = last["outcome"].as_str() {
        let ago = last["fired_ms"]
            .as_u64()
            .map(|at| format!(" {}", ago(now_ms().saturating_sub(at) / 1000)))
            .unwrap_or_default();
        let detail = match last["detail"].as_str() {
            Some(detail) if outcome == "failed" => {
                failed = Some(first_line(detail));
                String::new()
            }
            Some(detail) if outcome != "sent" => format!(": {}", first_line(detail)),
            _ => String::new(),
        };
        facts.push(format!("last {outcome}{ago}{detail}"));
        match last["turn_status"].as_str() {
            Some("completed") | None => {}
            // A turn the person stopped, or steered into another, ended as
            // they asked.
            Some(status) if status != "failed" => facts.push(format!("its turn {status}")),
            Some(_) => {
                facts.push("its turn failed".into());
                failed = Some(match last["turn_error"].as_str() {
                    Some(error) if !error.is_empty() => first_line(error),
                    _ => "its turn failed".into(),
                });
            }
        }
        if outcome == "failed" && failed.is_none() {
            failed = Some("failed".into());
        }
        // Its turn went well and its answer did not reach `--reply-to`.
        if last["reply"]["outcome"] == "failed" && failed.is_none() {
            facts.push("its answer was not passed on".into());
            failed = Some(match last["reply"]["detail"].as_str() {
                Some(detail) if !detail.is_empty() => first_line(detail),
                _ => "its answer was not passed on".into(),
            });
        }
    }
    if row["missed"] == true {
        facts.push("missed its time".into());
    }
    if row["ended"] == true {
        facts.push("ended".into());
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
        failed,
        handle: trigger::sent(row),
        fired_ms: last["fired_ms"].as_u64(),
    }
}

/// How many of `items` are for `bot`, Sonny's own left out.
pub fn count(items: &[Item], bot: &str) -> usize {
    items
        .iter()
        .filter(|item| !item.own && item.bots.iter().any(|b| b == bot))
        .count()
}

/// What `/status` says of `bot`'s: `↻ 2 automations, 1 follow`, or
/// nothing.
pub fn fact(items: &[Item], bot: &str) -> String {
    let of = |kind: Kind| {
        items
            .iter()
            .filter(|i| i.kind == kind && !i.own && i.bots.iter().any(|b| b == bot))
            .count()
    };
    let said: Vec<String> = [
        (of(Kind::Trigger), "automation"),
        (of(Kind::Subscription), "follow"),
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
    /// How the turns triggers' last fires sent ended, as the window last
    /// asked the daemon.
    turns: HashMap<String, Value>,
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
    pub fn open(
        scope: Scope,
        store: PathBuf,
        store_id: Option<String>,
        bot: String,
        turns: HashMap<String, Value>,
    ) -> Self {
        let mut view = Self {
            scope,
            store,
            store_id,
            bot,
            items: Vec::new(),
            turns,
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
        let mut items = read_scope(
            &self.store,
            self.store_id.as_deref(),
            &self.turns,
            &self.scope,
        );
        items.sort_by_key(|item| (item.failed.is_none(), item.kind != Kind::Trigger));
        self.items = items;
        self.selected = picked
            .and_then(|k| self.items.iter().position(|i| key_of(i) == k))
            .unwrap_or(self.selected)
            .min(self.items.len().saturating_sub(1));
        // A removal waits only on the row it was asked for, still picked.
        if self.confirm.is_some() && self.items.get(self.selected).map(key_of) != self.confirm {
            self.confirm = None;
        }
        self.lookout =
            crate::tool_host::with_status(lookout::status(&self.store, &self.bot), &self.store);
    }

    /// How the turns ended, as the window has since asked the daemon.
    pub fn set_turns(&mut self, turns: &HashMap<String, Value>) {
        if *turns != self.turns {
            self.turns = turns.clone();
            self.reload();
        }
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
            // find: never a run or a removal.
            KeyCode::Char(_)
                if key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) => {}
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(self.items.len().saturating_sub(1));
            }
            KeyCode::Char('r') => self.fire(),
            KeyCode::Char('d') | KeyCode::Delete => {
                let Some(item) = self.items.get(self.selected).cloned() else {
                    return Key::Stay;
                };
                if item.own {
                    self.notice = Some(
                        "Sonny keeps the lead's tasks follow; the lookout watches its tasks with it"
                            .into(),
                    );
                } else if item.kind == Kind::Server {
                    self.notice = Some(
                        "the nightly cleanup removes it the night the server reads again".into(),
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
            self.notice = Some("only an automation runs now".into());
            return;
        }
        let name = item.name.clone();
        self.notice = Some(
            match trigger::Places::home()
                .and_then(|places| trigger::fire_now(&places, &name, &trigger::ring))
            {
                Ok(_) => format!("ran {name}"),
                Err(error) => error,
            },
        );
        self.reload();
    }

    fn remove(&mut self, item: &Item) {
        let done = match item.kind {
            Kind::Trigger => {
                trigger::Places::home().and_then(|places| trigger::remove(&places, &item.name))
            }
            Kind::Subscription => {
                lookout::subs::remove(&self.store, &item.bot, &item.name).map(drop)
            }
            Kind::Server => return,
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
                    "An agent adds an automation with its triggers tool, and a follow with its subscriptions tool.",
                )
                .dim(),
            );
            return (out, 0);
        }
        // What failed first, whatever it is; then the rest by kind.
        let section = |item: &Item| match (&item.failed, item.kind) {
            (Some(_), _) => "Failed",
            (None, Kind::Subscription) => "Follows",
            (None, _) => "Automations",
        };
        for heading in ["Failed", "Automations", "Follows"] {
            let takes = |item: &Item| section(item) == heading;
            let n = self.items.iter().filter(|i| takes(i)).count();
            if n == 0 {
                continue;
            }
            out.push(Line::default());
            out.push(Line::from(vec![
                Span::raw(heading).bold(),
                Span::raw(format!(" · {n}")).dim(),
            ]));
            for (index, item) in self.items.iter().enumerate() {
                if !takes(item) {
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
        // The pick has a column of its own, as the composer's `› ` does,
        // so it never touches the row's mark; what follows lines up under
        // the name.
        // A column before the mark, as the picker's rows have, so the
        // pick reverses from the mark on.
        let pick = Span::raw("  ");
        // Sonny's one mark for what failed, as `/status` shows a task.
        let mark = match item.kind {
            _ if item.failed.is_some() => Span::raw("✗ ").fg(Color::Red),
            Kind::Trigger => Span::raw("↻ ").dim(),
            Kind::Subscription => Span::raw("◎ ").dim(),
            Kind::Server => Span::raw("  "),
        };
        let name = Span::raw(item.name.clone()).bold();
        let mut spans = vec![pick, mark, name];
        match item.kind {
            Kind::Trigger => {
                spans.push(Span::raw(format!(" {}", item.when)));
                spans.push(Span::raw(format!(" → {}", self.scope.short(&item.bot))).dim());
            }
            Kind::Subscription => {
                spans.push(Span::raw(format!(" for {}", self.scope.short(&item.bot))).dim());
            }
            Kind::Server => {
                spans.push(Span::raw(" could not be read by the nightly memory cleanup").dim());
            }
        }
        let mut first =
            crate::transcript::wrap(Line::from(spans), width, Span::raw(""), Span::raw(INDENT));
        if picked && let Some(row) = first.first_mut() {
            crate::list::pick(row);
        }
        out.extend(first);
        if let Some(why) = &item.failed {
            out.extend(crate::transcript::wrap(
                Line::from(why.clone()),
                width,
                Span::raw(INDENT),
                Span::raw(INDENT),
            ));
        }
        let mut facts = item.facts.clone();
        if let Some(to) = item.reply_to.as_ref().filter(|to| **to != item.bot) {
            facts.push(format!("answers go to {}", self.scope.short(to)));
        }
        if !facts.is_empty() {
            out.extend(crate::transcript::wrap(
                Line::from(facts.join(" · ")).dim(),
                width,
                Span::raw(INDENT),
                Span::raw(INDENT),
            ));
        }
        if !item.what.is_empty() {
            out.extend(crate::transcript::capped(
                Line::from(item.what.clone()).dim(),
                width,
                Span::raw(format!("{INDENT}└ ")).dim(),
                1,
            ));
        }
        out
    }

    /// The line under the card: what was done, the removal to confirm, or
    /// the keys the picked row takes.
    pub fn footer(&self) -> Option<Line<'static>> {
        if let Some((_, _, name)) = &self.confirm {
            return Some(Line::from(vec![
                Span::raw("Remove "),
                Span::raw(name.clone()).bold(),
                Span::raw("? It can't be undone.").dim(),
            ]));
        }
        self.notice
            .as_ref()
            .map(|notice| Line::from(notice.clone()))
    }

    /// What the keys do, as the pane's title says it: the picked row's,
    /// or the removal's while it waits to be confirmed.
    pub fn keys(&self) -> String {
        if self.confirm.is_some() {
            return "d remove · esc keep".into();
        }
        let mut keys = Vec::new();
        if self.items.len() > 1 {
            keys.push("↑↓ pick");
        }
        match self.items.get(self.selected) {
            Some(item) if item.kind == Kind::Trigger => keys.extend(["r run now", "d remove"]),
            Some(item) if !item.own && item.kind != Kind::Server => keys.push("d remove"),
            _ => {}
        }
        keys.push("esc close");
        keys.join(" · ")
    }
}

/// Where a row's lines after the first start: under its name, past the
/// pick and the mark.
const INDENT: &str = "    ";

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
            failed: None,
            handle: None,
            fired_ms: None,
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
        assert_eq!(fact(&items, "calc.lead"), "↻ 2 automations, 1 follow");
        assert_eq!(fact(&items, "calc.digest"), "↻ 1 automation");
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
        assert_eq!(item.facts, ["sent 4 of 10", "last failed 3h ago"]);
        assert_eq!(item.failed.as_deref(), Some("bot_busy: it works"));
        assert_eq!(item.reply_to.as_deref(), Some("calc.lead"));
        // A later fire that failed the same way, sending no turn, is news.
        let mut later = row.clone();
        later["last"]["fired_ms"] = json!(fired + 86_400_000);
        let later = trigger_item(&later, vec!["calc.lead".into()]);
        assert_ne!(failures(std::slice::from_ref(&item)), failures(&[later]));
        let fresh = trigger_item(
            &json!({"name": "new", "when": "fire", "bot": "calc.lead", "sent": 0, "last": null}),
            vec!["calc.lead".into()],
        );
        assert_eq!(fresh.facts, ["not run yet"]);
        assert_eq!(fresh.failed, None);
    }

    #[test]
    fn a_fire_that_went_out_fails_when_its_turn_did() {
        let row = |handle: &str| {
            json!({"name": "nightly", "when": "cron 0 5 * * *", "bot": "memory-cleanup",
                "message": "Clean up memory", "sent": 3,
                "last": {"outcome": "sent", "handle": handle, "fired_ms": now_ms()}})
        };
        let turns = HashMap::from([
            (
                "h1".to_owned(),
                json!({"status": "failed", "error": "provider_failed", "detail": "login expired\nsign in again"}),
            ),
            ("h2".to_owned(), json!({"status": "interrupted"})),
            (
                "h3".to_owned(),
                json!({"status": "completed", "text": "No changes."}),
            ),
        ]);
        let ended = |handle: &str| {
            let mut row = row(handle);
            trigger::with_turn(&mut row, &turns);
            trigger_item(&row, vec!["memory-cleanup".into()])
        };
        let failed = ended("h1");
        assert_eq!(
            failed.failed.as_deref(),
            Some("provider_failed: login expired…")
        );
        assert_eq!(
            failed.facts,
            ["sent 3", "last sent 0s ago", "its turn failed"]
        );
        assert_eq!(failed.handle.as_deref(), Some("h1"));
        // Stopped by the person: how it ended, not a failure.
        let stopped = ended("h2");
        assert_eq!(stopped.failed, None);
        assert_eq!(stopped.facts[2], "its turn interrupted");
        assert_eq!(ended("h3").failed, None);
        // Its answer did not reach the agent it was for.
        let mut lost = row("h3");
        lost["reply_to"] = json!("calc.lead");
        lost["last"]["reply"] = json!({"outcome": "failed", "detail": "bot_not_found: calc.lead"});
        trigger::with_turn(&mut lost, &turns);
        let lost = trigger_item(&lost, vec!["memory-cleanup".into()]);
        assert_eq!(lost.failed.as_deref(), Some("bot_not_found: calc.lead"));
        assert_eq!(lost.facts[2], "its answer was not passed on");
        // Not asked yet: the window asks the daemon for it.
        let running = ended("h4");
        assert_eq!(running.failed, None);
        assert_eq!(unended(&[running, failed], &turns), ["h4"]);
    }

    #[test]
    fn what_fails_for_the_person_shows_in_every_window_and_a_server_by_its_fact() {
        let scope = Scope {
            project: "calc".into(),
            lead: "calc.lead".into(),
            glob: "calc.*".into(),
        };
        let mut cleanup = item(Kind::Trigger, "memory", &["memory-cleanup"], false);
        assert!(!shown(&scope, &cleanup));
        cleanup.failed = Some("provider_failed".into());
        assert!(shown(&scope, &cleanup));
        let mut other = item(Kind::Trigger, "digest", &["web.lead"], false);
        other.failed = Some("bot_busy".into());
        assert!(!shown(&scope, &other));
        assert!(shown(
            &scope,
            &item(Kind::Trigger, "nightly", &["calc.lead"], false)
        ));

        let root = std::env::temp_dir().join(format!("background-servers-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let fact = |dir: &Path, name: &str, description: &str| {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(
                dir.join(format!("{name}.md")),
                format!("---\nname: {name}\ndescription: {description}\ntype: reference\nsource: memory-cleanup\nverified: 2026-10-30\n---\n\nsince 10-29\n"),
            )
            .unwrap();
        };
        fact(
            &root,
            "unreadable-chat",
            "chat server: auth_expired since 2026-10-29",
        );
        fact(&root, "prefers-short-replies", "George wants short replies");
        fact(
            &root.join("projects/calc"),
            "unreadable-mail",
            "mail server: timeout",
        );
        fact(
            &root.join("projects/web"),
            "unreadable-tracker",
            "tracker: 401",
        );
        let found = servers(&root, "calc");
        let names: Vec<&str> = found.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["chat", "mail"]);
        assert_eq!(
            found[0].failed.as_deref(),
            Some("chat server: auth_expired since 2026-10-29")
        );
        assert_eq!(found[0].facts[0], "memory fact unreadable-chat");
        assert_eq!(found[1].facts[0], "memory fact unreadable-mail in calc");
        assert!(
            found
                .iter()
                .all(|i| i.kind == Kind::Server && i.bots.is_empty())
        );
        assert_eq!(count(&found, "calc.lead"), 0);
        assert_eq!(failures(&found).len(), 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_card_lists_what_failed_first_marked_with_its_error() {
        let dir = std::env::temp_dir().join(format!("background-failed-{}", std::process::id()));
        let mut broken = item(Kind::Trigger, "memory", &["memory-cleanup"], false);
        broken.failed = Some("provider_failed: login expired".into());
        let mut server = item(Kind::Server, "chat", &["calc.lead"], false);
        (server.bot, server.bots, server.what) = (String::new(), Vec::new(), String::new());
        server.facts = vec!["memory fact unreadable-chat".into()];
        server.failed = Some("chat server: auth_expired".into());
        let mut view = View {
            scope: Scope {
                project: "calc".into(),
                lead: "calc.lead".into(),
                glob: "calc.*".into(),
            },
            store: dir.join("state.sqlite"),
            store_id: None,
            bot: "calc.lead".into(),
            items: vec![
                broken,
                server,
                item(Kind::Trigger, "nightly", &["calc.lead"], false),
            ],
            turns: HashMap::new(),
            lookout: String::new(),
            selected: 1,
            confirm: None,
            notice: None,
        };
        let (card, at) = view.card(100);
        let lines: Vec<String> = card.iter().map(|l| l.to_string()).collect();
        assert_eq!(lines[2], "Failed · 2");
        assert_eq!(lines[3], "  ✗ memory every 1d → memory-cleanup");
        let mark = card[3]
            .spans
            .iter()
            .find(|s| s.content.starts_with('✗'))
            .unwrap();
        assert_eq!(mark.style.fg, Some(Color::Red));
        assert_eq!(lines[4], "    provider_failed: login expired");
        assert_eq!(
            card[4].style.add_modifier,
            ratatui::style::Modifier::empty()
        );
        assert_eq!(
            lines[7],
            "  ✗ chat could not be read by the nightly memory cleanup "
        );
        assert_eq!(at, 7);
        assert_eq!(lines[8], "    chat server: auth_expired");
        assert_eq!(lines[9], "    memory fact unreadable-chat");
        assert_eq!(lines[11], "Automations · 1");
        assert_eq!(view.keys(), "↑↓ pick · esc close");
        assert_eq!(view.footer(), None);
        let press = |view: &mut View, code| view.key(KeyEvent::new(code, KeyModifiers::NONE));
        press(&mut view, KeyCode::Char('d'));
        assert_eq!(view.confirm, None);
        assert!(view.notice.as_deref().unwrap().contains("reads again"));
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
            turns: HashMap::new(),
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
        assert_eq!(lines[3], "Automations · 1");
        assert_eq!(lines[4], "  ↻ nightly every 1d → lead ");
        assert_eq!(lines[5], "    sent 2");
        assert_eq!(lines[6], "    └ Read preferences.md");
        assert_eq!(lines[8], "Follows · 1");
        assert_eq!(lines[9], "  ◎ tasks for lead");
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
