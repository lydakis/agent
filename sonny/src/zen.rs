//! Zen: one agent, one focus, as the person's phone keeps a Focus. `/zen
//! FOCUS` goes to the agent as the person typed it, and its `zen` skill
//! says how to talk only about that and hold the rest back; `/zen off`
//! ends it, and the agent tells what it held. While it is on, the window
//! shows only the turns the person started and the agent's questions.
//! Kept per agent in `~/.agent/sonny/zen/NAME`, so every window of the
//! agent and the commons' invites know it.
use std::path::{Path, PathBuf};

/// The skill every agent is given.
pub const SKILL: &str = include_str!("../skills/zen/SKILL.md");

/// What `/zen` asks for.
#[derive(Debug, PartialEq)]
pub enum Command {
    /// `/zen FOCUS`: on, with this focus.
    On(String),
    /// `/zen off`, or `/zen` while on.
    Off,
}

/// `prompt` read as a `/zen` command, given whether zen is on: `/zen`
/// alone leaves it, and asks for a focus when it is off.
pub fn command(prompt: &str, on: bool) -> Option<Result<Command, &'static str>> {
    let rest = prompt.strip_prefix("/zen")?;
    if !(rest.is_empty() || rest.starts_with(char::is_whitespace)) {
        return None;
    }
    Some(match rest.trim() {
        "" if on => Ok(Command::Off),
        "" => Err("/zen FOCUS: say the one thing to talk about"),
        "off" => match on {
            true => Ok(Command::Off),
            false => Err("zen is not on"),
        },
        focus => Ok(Command::On(focus.to_owned())),
    })
}

fn file(home: &Path, bot: &str) -> Option<PathBuf> {
    (!bot.is_empty() && !bot.contains('/') && !bot.starts_with('.'))
        .then(|| home.join(".agent/sonny/zen").join(bot))
}

#[cfg(not(test))]
fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Tests keep their zen apart from the person's.
#[cfg(test)]
fn home() -> Option<PathBuf> {
    Some(std::env::temp_dir().join(format!("sonny-zen-home-{}", std::process::id())))
}

/// The focus `bot` is in zen on, if it is.
pub fn focus(bot: &str) -> Option<String> {
    focus_in(&home()?, bot)
}

pub fn focus_in(home: &Path, bot: &str) -> Option<String> {
    let text = std::fs::read_to_string(file(home, bot)?).ok()?;
    Some(text.trim().to_owned()).filter(|focus| !focus.is_empty())
}

/// Zen on `bot` with `focus`, or off with none.
pub fn set(bot: &str, focus: Option<&str>) -> Result<(), String> {
    let home = home().ok_or("zen: no HOME")?;
    set_in(&home, bot, focus)
}

pub fn set_in(home: &Path, bot: &str, focus: Option<&str>) -> Result<(), String> {
    let path = file(home, bot).ok_or_else(|| format!("zen: no agent named {bot:?}"))?;
    match focus {
        Some(focus) => agent_client::files::replace_mode(&path, focus.as_bytes(), 0o600)
            .map_err(|e| format!("zen: {e}")),
        None => match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(format!("zen: {}: {e}", path.display()))
            }
            _ => Ok(()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zen_takes_a_focus_and_alone_or_off_leaves() {
        let on = |focus: &str| Some(Ok(Command::On(focus.into())));
        assert_eq!(command("/zen fix the tests", false), on("fix the tests"));
        assert_eq!(command("/zen  the receipts ", true), on("the receipts"));
        assert_eq!(command("/zen", true), Some(Ok(Command::Off)));
        assert_eq!(command("/zen off", true), Some(Ok(Command::Off)));
        assert!(matches!(command("/zen", false), Some(Err(_))));
        assert!(matches!(command("/zen off", false), Some(Err(_))));
        assert_eq!(command("/zenith", false), None);
        assert_eq!(command("zen", false), None);
    }

    #[test]
    fn the_focus_is_kept_per_agent_until_zen_is_left() {
        let home = std::env::temp_dir().join(format!("sonny-zen-{}", std::process::id()));
        assert_eq!(focus_in(&home, "shop.lead"), None);
        set_in(&home, "shop.lead", Some("the receipts")).unwrap();
        assert_eq!(
            focus_in(&home, "shop.lead").as_deref(),
            Some("the receipts")
        );
        assert_eq!(focus_in(&home, "shop.fix"), None);
        set_in(&home, "shop.lead", None).unwrap();
        set_in(&home, "shop.lead", None).unwrap();
        assert_eq!(focus_in(&home, "shop.lead"), None);
        assert!(set_in(&home, "../x", Some("no")).is_err());
        let _ = std::fs::remove_dir_all(&home);
    }
}
