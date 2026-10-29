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

/// Every key `agent_client::config` reads.
pub const SETTINGS: [Setting; 8] = [
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
        about: "whether new agents search the web where their provider offers it",
        values: &["live", "disabled"],
        default: "live",
    },
    Setting {
        key: "judge.model",
        about: "who judges for auto approvals and the lookout (provider/model)",
        values: &[],
        default: "the agent's own model",
    },
    Setting {
        key: "judge.effort",
        about: "the judge's effort",
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
        key: "sonny.side_chat",
        about: "/side's copy answers calling no tool, or is a fork that can do anything its agent can",
        values: &["answer", "fork"],
        default: "answer",
    },
];

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
                value.and_then(|v| v.as_str()).map(str::to_owned)
            })
            .collect())
    }

    /// Writes the picked setting's `value`, `None` for its default, and
    /// reads the file again.
    fn write(&mut self, value: Option<String>) {
        let key = SETTINGS[self.pick].key;
        let written = agent_client::config::write(&self.path, &[(key, value.as_deref())])
            .and_then(|()| self.read());
        self.said = Some(match written {
            Ok(values) => {
                self.values = values;
                Ok(match value {
                    Some(value) => format!("{key} = \"{value}\""),
                    None => format!("{key} is its default again"),
                })
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

    /// Up to `room` settings around the pick, each with its value.
    pub fn rows(&self, room: usize) -> Vec<Line<'static>> {
        let shown = SETTINGS.len().min(room.max(1));
        let first = (self.pick + 1).saturating_sub(shown);
        let wide = SETTINGS.iter().map(|s| s.key.len()).max().unwrap_or(0);
        (SETTINGS.iter().zip(&self.values))
            .enumerate()
            .skip(first)
            .take(shown)
            .map(|(i, (setting, value))| {
                let key = format!("{:<wide$}", setting.key);
                let value = match value {
                    Some(value) => Span::raw(format!("  {value}")),
                    None => Span::raw(format!("  {} (default)", setting.default)).dim(),
                };
                if i == self.pick {
                    let value = match &self.typing {
                        Some(typed) => Span::raw(format!("  {typed}")),
                        None => value,
                    };
                    Line::from(vec![
                        Span::raw("  "),
                        Span::raw(format!(" {key}")).reversed(),
                        value.reversed(),
                        Span::raw(" ").reversed(),
                    ])
                } else {
                    Line::from(vec![Span::raw(format!("   {key}")), value])
                }
            })
            .collect()
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
            Some(Err(error)) => Span::raw(error.clone()).fg(Color::Red),
            None => Span::raw(SETTINGS[self.pick].about).dim(),
        };
        (Line::from(vec![Span::raw("› ").bold(), what]), None)
    }

    /// The keys the picked setting takes.
    pub fn keys(&self) -> Line<'static> {
        let file = crate::tilde(&self.path.display().to_string());
        let keys = match (&self.typing, SETTINGS[self.pick].values.is_empty()) {
            (Some(_), _) => "  enter to save, empty for the default · esc".to_owned(),
            (None, true) => format!("  ↑↓ setting · enter to type · ⌫ default · esc · {file}"),
            (None, false) => format!("  ↑↓ setting · ←→ value · ⌫ default · esc · {file}"),
        };
        Line::from(keys).dim()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyEvent;

    fn press(view: &mut View, code: KeyCode) {
        view.event(&Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    #[test]
    fn every_key_the_file_takes_is_listed() {
        // A value that is no key's would be refused by the reader.
        let path = std::env::temp_dir().join(format!("sonny-config-keys-{}", std::process::id()));
        for setting in &SETTINGS {
            let value = setting.values.first().copied().unwrap_or("a/b");
            agent_client::config::set(&path, &[(setting.key, value)]).unwrap();
        }
        std::fs::remove_file(&path).unwrap();
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
        assert_eq!(view.values[5].as_deref(), Some("low"));
        // The side chat: a fork, then its default again either way.
        view.pick = 7;
        press(&mut view, KeyCode::Right);
        assert_eq!(view.values[7].as_deref(), Some("fork"));
        press(&mut view, KeyCode::Right);
        assert_eq!(view.values[7], None);
        press(&mut view, KeyCode::Left);
        assert_eq!(view.values[7].as_deref(), Some("fork"));
        // Efforts step through every level.
        view.pick = 4;
        press(&mut view, KeyCode::Left);
        assert_eq!(view.values[4].as_deref(), Some("max"));
        press(&mut view, KeyCode::Enter);
        assert_eq!(view.values[4], None);
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
        view.pick = 5;
        press(&mut view, KeyCode::Backspace);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# mine\n[sonny]\nside_chat = \"fork\"\n"
        );
        // A value the reader refuses is said, and the file kept.
        view.pick = 6;
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
