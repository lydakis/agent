//! `/config`: the person's `~/.agent/config.toml`, read and changed from
//! Sonny, every key the file takes in one list. A key with a few values
//! steps through them, and one that takes a name is typed; either is
//! written to the file at once, the person's comments and order kept, and
//! a key taken back to its default has its line taken out.
use ratatui::crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    style::{Color, Stylize},
    text::{Line, Span},
};
use std::path::PathBuf;
use unicode_width::UnicodeWidthStr;

/// One key of the file.
pub struct Setting {
    /// As the file names it: `table.key`, or the bare key.
    pub key: &'static str,
    /// What it sets, and when that is read.
    pub about: &'static str,
    /// The values it takes, or none for one that takes a name.
    pub values: &'static [&'static str],
    /// What an unset key is.
    pub default: &'static str,
}

const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];
/// A switch, written as TOML's own true or false.
const SWITCH: &[&str] = &["true", "false"];

/// Whether a setting takes a whole number, typed: its default is one.
fn number(setting: &Setting) -> bool {
    setting.values.is_empty() && setting.default.parse::<u64>().is_ok()
}

/// Whether a setting is a span of time, as `30s`, `15m` or `1h`.
fn span(setting: &Setting) -> bool {
    setting.values.is_empty()
        && !number(setting)
        && agent_client::config::span(setting.key, setting.default).is_ok()
}

/// The rows `/config` takes with room for all: the blank one, a heading
/// for each table, and the settings.
pub fn height() -> usize {
    let mut tables: Vec<&str> = SETTINGS
        .iter()
        .map(|s| s.key.split_once('.').map_or("", |(table, _)| table))
        .collect();
    tables.dedup();
    1 + tables.len() + SETTINGS.len()
}

/// A key without its table, which its heading names.
fn short(key: &str) -> &str {
    key.split_once('.').map_or(key, |(_, key)| key)
}

/// Every key `agent_client::config` reads.
pub const SETTINGS: [Setting; 31] = [
    Setting {
        key: "model",
        about: "a new agent's model, when none is named (provider/model)",
        values: &[],
        default: "none",
    },
    Setting {
        key: "provider",
        about: "the daemon's providers, as AGENT_PROVIDER takes them; read when it starts",
        values: &[],
        default: "AGENT_PROVIDER",
    },
    Setting {
        key: "web_search",
        about: "whether new agents search and fetch the web where their provider offers it",
        values: &["live", "disabled"],
        default: "live",
    },
    Setting {
        key: "senders",
        about: "whether a new agent's model reads a line naming who sent it each message",
        values: &["shown", "hidden"],
        default: "shown",
    },
    Setting {
        key: "judge.model",
        about: "the reviewer for auto approvals (provider/model)",
        values: &[],
        default: "the agent's own model",
    },
    Setting {
        key: "judge.effort",
        about: "the reviewer's effort",
        values: EFFORTS,
        default: "the model's own",
    },
    Setting {
        key: "sonny.lead_effort",
        about: "a new lead's effort",
        values: EFFORTS,
        default: "the model's own",
    },
    Setting {
        key: "sonny.task_effort",
        about: "each new task's effort",
        values: EFFORTS,
        default: "its lead's",
    },
    Setting {
        key: "sonny.lead_profile",
        about: "the profile a new lead runs as",
        values: &[],
        default: "lead",
    },
    Setting {
        key: "sonny.task_profile",
        about: "the profile every other agent Sonny starts is told",
        values: &[],
        default: "task",
    },
    Setting {
        key: "sonny.side_chat",
        about: "/side's copy answers calling no tool, or is a fork that can do anything its agent can",
        values: &["answer", "fork"],
        default: "answer",
    },
    Setting {
        key: "sonny.chime",
        about: "whether finished and waiting work rings; notifications show either way",
        values: SWITCH,
        default: "true",
    },
    Setting {
        key: "sonny.verbose",
        about: "every tool call drawn, or the clean view: replies, tasks, questions and the plan",
        values: SWITCH,
        default: "false",
    },
    Setting {
        key: "sonny.text_width",
        about: "the widest the conversation is written, in columns, however wide the window, the pane beside it taking the rest; 0 for the window's whole width, a pane then opening over it",
        values: &[],
        default: "100",
    },
    Setting {
        key: "sonny.margin_opens",
        about: "when a note the agent puts up or changes opens the margin: always, unless you closed it with Esc, or never",
        values: &["always", "unless-closed", "never"],
        default: "always",
    },
    Setting {
        key: "sonny.agent_opens",
        about: "where a click on an agent's name takes you: its tab in Rex, Herdr or cmux, or a look beside the conversation; a right click, the other",
        values: &["tab", "beside"],
        default: "tab",
    },
    Setting {
        key: "sonny.suggest",
        about: "after a turn you started, a copy of the agent guesses your next message for Tab to take; one small call",
        values: SWITCH,
        default: "true",
    },
    Setting {
        key: "sonny.start_at_boot",
        about: "the daemon starts when this machine does (at login on a Mac), so a box's agents keep working after a restart",
        values: SWITCH,
        default: "false",
    },
    Setting {
        key: "sonny.commons",
        about: "whether agents have the commons: its tool, its skill and the roles' lines about it; read when Sonny starts",
        values: SWITCH,
        default: "true",
    },
    Setting {
        key: "sonny.commons_invites",
        about: "least commons threads a day that send invites, the square root of the week's agents if more, 0 for none; each answered thread adds one, up to as many again",
        values: &[],
        default: "3",
    },
    Setting {
        key: "sonny.commons_invite_tokens",
        about: "the tokens an invite pays for in its thread: its turn, and the turns replies there start while any are left",
        values: &[],
        default: "1000000",
    },
    Setting {
        key: "sonny.commons_digest",
        about: "how often at most an agent hears of new posts in the commons threads it posted in, as 30s, 15m or 1h; 0 for as they come",
        values: &[],
        default: "1h",
    },
    Setting {
        key: "sonny.ask_wait_minutes",
        about: "minutes an unanswered question waits before its agent goes on with what it recommended, where it said it could, or is reminded; 0 waits for you always",
        values: &[],
        default: "60",
    },
    Setting {
        key: "sonny.default_permissions",
        about: "the /permissions row a new lead starts in; a [sonny.permissions] row of your own is typed into the file",
        values: &["auto", "ask", "read-only", "full"],
        default: "auto",
    },
    Setting {
        key: "sonny.task_windows",
        about: "task windows a lead keeps open in its multiplexer: past it, a new one closes the one you used least recently, never one you have not looked at yet, that needs you, failed or runs something it asked for (with only unseen ones to close, the new one waits for /status); 0 for no limit",
        values: &[],
        default: "32",
    },
    Setting {
        key: "sonny.task_depth",
        about: "how many generations of tasks a lead's line has: 2 lets its tasks start their own, which start none; 0 for no end",
        values: &[],
        default: "2",
    },
    Setting {
        key: "sonny.sky_haikus",
        about: "haikus a day from the agent the sky finds busiest, each written by a copy of it that calls no tool when the title passes; 0 for none",
        values: &[],
        default: "8",
    },
    Setting {
        key: "sonny.sky_haiku_window",
        about: "the minutes of messages and tool calls that make an agent the sky's busiest",
        values: &[],
        default: "10",
    },
    Setting {
        key: "sonny.sky_haiku_margin",
        about: "how much busier, in percent, an agent must be than the busiest to take the title, so it does not flicker",
        values: &[],
        default: "50",
    },
    Setting {
        key: "sonny.sky_haiku_hold",
        about: "the minutes the busiest keeps the title before another can take it",
        values: &[],
        default: "10",
    },
    Setting {
        key: "sonny.tool_processes",
        about: "processes Sonny runs for agents at once in all: the tool host's tool calls take three quarters, the lookout's goal checks a quarter, each at least one, and each agent at most half of either; 0 for four times the CPUs",
        values: &[],
        default: "0",
    },
];

/// The `sonny-config` skill: where the settings live and every key of the
/// file, from [`SETTINGS`], so it says what this Sonny reads.
pub fn skill() -> String {
    let rows: Vec<String> = SETTINGS
        .iter()
        .map(|setting| {
            let values = match setting.values {
                [] if number(setting) => "a whole number".to_owned(),
                [] if span(setting) => "a span, as `30s`, `15m` or `1h`".to_owned(),
                [] => "a name".to_owned(),
                values => (values.iter())
                    .map(|value| format!("`{value}`"))
                    .collect::<Vec<_>>()
                    .join(", "),
            };
            format!(
                "| `{}` | {} | {values} | {} |",
                setting.key, setting.about, setting.default
            )
        })
        .collect();
    let table = format!(
        "| Key | What it sets | Values | Unset |\n| --- | --- | --- | --- |\n{}",
        rows.join("\n")
    );
    include_str!("../skills/sonny-config/SKILL.md")
        .replace("{{VERSION}}", env!("CARGO_PKG_VERSION"))
        .replace("{{SETTINGS}}", &table)
}

/// What the keys say: open in `/config`.
pub enum Key {
    Stay,
    Close,
}

pub struct View {
    path: PathBuf,
    /// Each setting's value in the file, `None` for its default.
    values: Vec<Option<String>>,
    pick: usize,
    /// The name being typed for the picked setting.
    typing: Option<String>,
    /// What the last change came to: saved, or why not.
    said: Option<Result<String, String>>,
}

impl View {
    /// The file as it is; an unreadable or invalid one is refused, as
    /// every other reader refuses it.
    pub fn open() -> Result<Self, String> {
        let path = agent_client::config::path().ok_or("HOME is not set")?;
        let mut view = View {
            path,
            values: Vec::new(),
            pick: 0,
            typing: None,
            said: None,
        };
        view.values = view.read()?;
        Ok(view)
    }

    fn read(&self) -> Result<Vec<Option<String>>, String> {
        agent_client::config::read(&self.path)?;
        let text = std::fs::read_to_string(&self.path).unwrap_or_default();
        let table: toml::Table = text.parse().unwrap_or_default();
        Ok(SETTINGS
            .iter()
            .map(|setting| {
                let value = match setting.key.split_once('.') {
                    Some((name, key)) => table.get(name).and_then(|t| t.get(key)),
                    None => table.get(setting.key),
                };
                value.and_then(|v| match v {
                    toml::Value::Boolean(on) => Some(on.to_string()),
                    toml::Value::Integer(n) => Some(n.to_string()),
                    v => v.as_str().map(str::to_owned),
                })
            })
            .collect())
    }

    /// Writes the picked setting's `value`, `None` for its default, and
    /// reads the file again.
    fn write(&mut self, value: Option<String>) {
        let setting = &SETTINGS[self.pick];
        let value = value.map(|value| match (setting.values == SWITCH, number(setting)) {
            (true, _) => toml::Value::Boolean(value == "true"),
            (_, true) => value
                .parse()
                .map_or(toml::Value::String(value), toml::Value::Integer),
            _ => toml::Value::String(value),
        });
        let written = agent_client::config::write(&self.path, &[(setting.key, value.clone())])
            .and_then(|()| self.read());
        self.said = Some(match written {
            Ok(values) => {
                self.values = values;
                let said = match &value {
                    Some(value) => format!("{} = {value}", setting.key),
                    None => format!("{} is its default again", setting.key),
                };
                // The setting is the job: it is set up or taken away now.
                match setting.key {
                    "sonny.start_at_boot" => {
                        let on = value.as_ref().and_then(toml::Value::as_bool) == Some(true);
                        crate::boot::apply(on).map(|done| format!("{said}: {done}"))
                    }
                    _ => Ok(said),
                }
            }
            Err(error) => Err(error),
        });
    }

    /// The next of the picked setting's values, `by` steps on, through
    /// its default and round again.
    fn step(&mut self, by: isize) {
        let setting = &SETTINGS[self.pick];
        if setting.values.is_empty() {
            return;
        }
        // The default first, then each value that is not the default
        // written out.
        let values: Vec<&str> = (setting.values.iter())
            .copied()
            .filter(|value| *value != setting.default)
            .collect();
        let now = match &self.values[self.pick] {
            None => 0,
            Some(value) => (values.iter())
                .position(|v| v == value)
                .map_or(0, |at| at as isize + 1),
        };
        let next = (now + by).rem_euclid(values.len() as isize + 1) as usize;
        self.write(next.checked_sub(1).map(|at| values[at].to_owned()));
    }

    pub fn event(&mut self, event: &Event) -> Key {
        if let Event::Paste(text) = event
            && let Some(typed) = &mut self.typing
        {
            typed.push_str(text.lines().next().unwrap_or_default());
        }
        let Event::Key(key) = event else {
            return Key::Stay;
        };
        if key.kind == KeyEventKind::Release {
            return Key::Stay;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Key::Close;
        }
        let named = SETTINGS[self.pick].values.is_empty();
        if let Some(typed) = &mut self.typing {
            match key.code {
                KeyCode::Char(c) => typed.push(c),
                KeyCode::Backspace => {
                    typed.pop();
                }
                KeyCode::Esc => self.typing = None,
                KeyCode::Enter => {
                    let typed = self.typing.take().unwrap_or_default();
                    let typed = typed.trim();
                    self.write((!typed.is_empty()).then(|| typed.to_owned()));
                }
                _ => {}
            }
            return Key::Stay;
        }
        self.said = None;
        match key.code {
            KeyCode::Esc => return Key::Close,
            KeyCode::Up => self.pick = self.pick.saturating_sub(1),
            KeyCode::Down => self.pick = (self.pick + 1).min(SETTINGS.len() - 1),
            KeyCode::Left => self.step(-1),
            KeyCode::Right => self.step(1),
            KeyCode::Enter if named => {
                self.typing = Some(self.values[self.pick].clone().unwrap_or_default())
            }
            KeyCode::Enter => self.step(1),
            KeyCode::Backspace | KeyCode::Delete if self.values[self.pick].is_some() => {
                self.write(None)
            }
            // Typing a name starts it afresh.
            KeyCode::Char(c) if named => self.typing = Some(c.to_string()),
            _ => {}
        }
        Key::Stay
    }

    /// Every setting under its table's name, each with its value, under a
    /// blank row: up to `room` rows, around the pick. A value set in the
    /// file reads plain and a default muted, so what was changed stands
    /// out.
    pub fn rows(&self, room: usize) -> Vec<Line<'static>> {
        let wide = SETTINGS
            .iter()
            .map(|s| short(s.key).len())
            .max()
            .unwrap_or(0);
        let mut out = vec![Line::default()];
        let mut picked = 0;
        let mut table = "";
        for (i, (setting, value)) in SETTINGS.iter().zip(&self.values).enumerate() {
            let (this, _) = setting.key.split_once('.').unwrap_or(("", setting.key));
            if this != table || i == 0 {
                table = this;
                let name = match this {
                    "" => "agent".to_owned(),
                    this => format!("[{this}]"),
                };
                out.push(Line::from(format!("    {name}")).dim().bold());
            }
            if i == self.pick {
                picked = out.len();
            }
            out.push(self.row(i, setting, value, wide));
        }
        let shown = out.len().min(room.max(1));
        let first = (picked + 1).saturating_sub(shown);
        out.into_iter().skip(first).take(shown).collect()
    }

    /// The setting `i`'s row, its key `wide` wide.
    fn row(
        &self,
        i: usize,
        setting: &Setting,
        value: &Option<String>,
        wide: usize,
    ) -> Line<'static> {
        let key = format!("{:<wide$}", short(setting.key));
        let value = match value {
            Some(value) => Span::raw(format!("  {value}")),
            None => Span::raw(format!("  {}", setting.default)).dim(),
        };
        if i != self.pick {
            return Line::from(vec![Span::raw(format!("      {key}")), value]);
        }
        let value = match &self.typing {
            Some(typed) => Span::raw(format!("  {typed}")),
            None => value,
        };
        Line::from(vec![
            Span::raw("      "),
            Span::raw(key).reversed(),
            value.reversed(),
            Span::raw(" ").reversed(),
        ])
    }

    /// The composer's line: what the picked setting sets, what the last
    /// change came to, or the name being typed; and where the cursor goes
    /// when one is.
    pub fn line(&self) -> (Line<'static>, Option<u16>) {
        if let Some(typed) = &self.typing {
            let line = Line::from(vec![Span::raw("› ").bold(), Span::raw(typed.clone())]);
            return (line, Some(2 + typed.width() as u16));
        }
        let what = match &self.said {
            Some(Ok(saved)) => Span::raw(format!("saved {saved}")).fg(Color::Green),
            Some(Err(error)) => Span::raw(format!("✗ {error}")),
            None => Span::raw(SETTINGS[self.pick].about).dim(),
        };
        (Line::from(vec![Span::raw("› ").bold(), what]), None)
    }

    /// The keys the picked setting takes.
    pub fn keys(&self) -> Line<'static> {
        let file = crate::tilde(&self.path.display().to_string());
        let keys = match (&self.typing, SETTINGS[self.pick].values.is_empty()) {
            (Some(_), _) => "  enter save, empty for the default · esc cancel",
            (None, true) => "  ↑↓ pick · enter type · ⌫ default · esc close",
            (None, false) => "  ↑↓ pick · ←→ change · ⌫ default · esc close",
        };
        let mut spans = crate::hint::spans(keys);
        if self.typing.is_none() {
            spans.push(Span::raw(format!("  {file}")).dim());
        }
        Line::from(spans)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_config_skill_lists_every_key_as_this_sonny_reads_it() {
        let skill = skill();
        assert!(skill.starts_with("---\nname: sonny-config\n"));
        assert!(!skill.contains("{{"));
        assert!(skill.contains(env!("CARGO_PKG_VERSION")));
        for setting in &SETTINGS {
            assert!(
                skill.contains(&format!("| `{}` |", setting.key)),
                "{}",
                setting.key
            );
        }
        assert!(skill.contains("| `sonny.margin_opens` |"));
        assert!(skill.contains("`always`, `unless-closed`, `never` | always |"));
        assert!(skill.contains("| `sonny.commons_invites` |"));
        assert!(skill.contains("| a whole number | 3 |"));
    }
    use ratatui::crossterm::event::KeyEvent;

    fn press(view: &mut View, code: KeyCode) {
        view.event(&Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    #[test]
    fn every_key_the_file_takes_is_listed() {
        // A key or value the reader does not take would be refused.
        let dir = std::env::temp_dir().join(format!("sonny-config-keys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut view = View {
            path: dir.join("config.toml"),
            values: vec![None; SETTINGS.len()],
            pick: 0,
            typing: None,
            said: None,
        };
        for (at, setting) in SETTINGS.iter().enumerate() {
            view.pick = at;
            let named = match (number(setting), span(setting)) {
                // Wide enough for a text width.
                (true, _) => "70",
                (_, true) => "15m",
                _ => "a/b",
            };
            let value = setting.values.last().copied().unwrap_or(named);
            view.write(Some(value.to_owned()));
            assert!(matches!(view.said, Some(Ok(_))), "{:?}", view.said);
            assert_eq!(view.values[at].as_deref(), Some(value));
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn values_step_through_the_default_and_names_are_typed() {
        let dir = std::env::temp_dir().join(format!("sonny-config-view-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "# mine\n[sonny]\nlead_effort = \"low\"\n").unwrap();
        let mut view = View {
            path: path.clone(),
            values: Vec::new(),
            pick: 0,
            typing: None,
            said: None,
        };
        view.values = view.read().unwrap();
        assert_eq!(view.values[6].as_deref(), Some("low"));
        // The side chat: a fork, then its default again either way.
        view.pick = 10;
        press(&mut view, KeyCode::Right);
        assert_eq!(view.values[10].as_deref(), Some("fork"));
        press(&mut view, KeyCode::Right);
        assert_eq!(view.values[10], None);
        press(&mut view, KeyCode::Left);
        assert_eq!(view.values[10].as_deref(), Some("fork"));
        // Efforts step through every level.
        view.pick = 5;
        press(&mut view, KeyCode::Left);
        assert_eq!(view.values[5].as_deref(), Some("max"));
        press(&mut view, KeyCode::Enter);
        assert_eq!(view.values[5], None);
        // A model is typed whole, and an empty one is the default.
        view.pick = 0;
        for c in "a/b".chars() {
            press(&mut view, KeyCode::Char(c));
        }
        assert_eq!(view.values[0], None);
        press(&mut view, KeyCode::Enter);
        assert_eq!(view.values[0].as_deref(), Some("a/b"));
        press(&mut view, KeyCode::Enter);
        for _ in 0..3 {
            press(&mut view, KeyCode::Backspace);
        }
        press(&mut view, KeyCode::Enter);
        assert_eq!(view.values[0], None);
        // The lead's effort back to its default.
        view.pick = 6;
        press(&mut view, KeyCode::Backspace);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# mine\n[sonny]\nside_chat = \"fork\"\n"
        );
        // A switch is written as TOML's own.
        view.pick = 11;
        press(&mut view, KeyCode::Right);
        assert_eq!(view.values[11].as_deref(), Some("false"));
        assert!(!agent_client::config::read(&path).unwrap().sonny.chime);
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .ends_with("chime = false\n")
        );
        press(&mut view, KeyCode::Right);
        assert_eq!(view.values[11], None);
        // A value the reader refuses is said, and the file kept.
        view.pick = 7;
        view.typing = None;
        std::fs::write(&path, "[sonny]\nbogus = \"x\"\n").unwrap();
        press(&mut view, KeyCode::Right);
        assert!(matches!(&view.said, Some(Err(e)) if e.contains("unknown key sonny.bogus")));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[sonny]\nbogus = \"x\"\n"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
