//! Zen: one agent, one focus, as the person's phone keeps a Focus. `/zen
//! FOCUS` goes to the agent as the person typed it, and its `zen` skill
//! says how to talk only about that and hold the rest back; `/zen off`
//! ends it, and the agent tells what it held. While it is on, the window
//! shows only the turns the person started and the agent's questions.
//! Kept per agent in `~/.agent/sonny/zen/KEY`, KEY the agent's store and
//! durable id as [`crate::reactions::key`] makes it, so every window of
//! the agent and the commons' invites know it, and another store's agent
//! of the same name, or one made again under it, does not. The agent sets
//! it too, with `sonny zen`, when the person asks in words or its reading
//! of the focus differs from what they typed.
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

fn file(home: &Path, key: &str) -> Option<PathBuf> {
    (!key.is_empty() && !key.contains('/') && !key.starts_with('.'))
        .then(|| home.join(".agent/sonny/zen").join(key))
}

/// The folder of every agent's focus, made when missing: one a sandboxed
/// agent may write, so `sonny zen` works from its shell. Only foci are kept
/// there.
pub fn folder() -> Option<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let folder = home()?.join(".agent/sonny/zen");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&folder)
        .ok()?;
    Some(folder)
}

#[cfg(not(test))]
fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Tests keep their zen apart from the person's, and from each other's.
#[cfg(test)]
fn home() -> Option<PathBuf> {
    let thread = format!("{:?}", std::thread::current().id());
    let thread: String = thread.chars().filter(char::is_ascii_digit).collect();
    Some(std::env::temp_dir().join(format!("sonny-zen-{}-{thread}", std::process::id())))
}

/// The focus the agent keyed `key` is in zen on, if it is; none with no
/// key.
pub fn focus(key: Option<&str>) -> Option<String> {
    focus_in(&home()?, key?)
}

/// The focus kept for `key` under `home`. A sandboxed agent may write the
/// folder, so only a plain file of at most [`MAX_FOCUS`] bytes is read, and
/// never through a link: a pipe, a device or a link to one would otherwise
/// hold the window that asks, which reads it on its own loop.
pub fn focus_in(home: &Path, key: &str) -> Option<String> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(file(home, key)?)
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut text = String::new();
    file.take(MAX_FOCUS as u64 + 1)
        .read_to_string(&mut text)
        .ok()?;
    (text.len() <= MAX_FOCUS)
        .then(|| text.trim().to_owned())
        .filter(|focus| !focus.is_empty())
}

/// The most a focus file is read: far past any focus a person types.
pub const MAX_FOCUS: usize = 16 * 1024;

/// Whether any agent is in zen, before asking which.
pub fn any_in(home: &Path) -> bool {
    std::fs::read_dir(home.join(".agent/sonny/zen")).is_ok_and(|mut dir| dir.next().is_some())
}

/// Zen on the agent keyed `key` with `focus`, or off with none.
pub fn set(key: Option<&str>, focus: Option<&str>) -> Result<(), String> {
    let home = home().ok_or("zen: no HOME")?;
    set_in(
        &home,
        key.ok_or("zen: this agent has no id to keep it by")?,
        focus,
    )
}

pub fn set_in(home: &Path, key: &str, focus: Option<&str>) -> Result<(), String> {
    let path = file(home, key).ok_or_else(|| format!("zen: no agent keyed {key:?}"))?;
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

pub const USAGE: &str = "usage: sonny zen FOCUS | sonny zen off (run by an agent, for itself)";

/// `sonny zen FOCUS` or `sonny zen off`, run by an agent from its shell:
/// zen for itself, found by the store and id its turn was given.
pub fn cli(args: &[String]) -> Result<(), String> {
    let focus = args.join(" ");
    let focus = match focus.trim() {
        "" => return Err(USAGE.into()),
        "off" => None,
        focus => Some(focus),
    };
    let var = |name| std::env::var(name).ok().filter(|v: &String| !v.is_empty());
    let (Some(store), Some(id)) = (var("AGENT_STORE"), var("AGENT_BOT_ID")) else {
        return Err(USAGE.into());
    };
    let id: i64 = id.parse().map_err(|_| USAGE.to_owned())?;
    let store = std::fs::canonicalize(&store).unwrap_or_else(|_| PathBuf::from(&store));
    set(Some(&crate::reactions::key(&store, id)), focus)
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
        assert_eq!(focus_in(&home, "1a-3"), None);
        set_in(&home, "1a-3", Some("the receipts")).unwrap();
        assert_eq!(focus_in(&home, "1a-3").as_deref(), Some("the receipts"));
        assert_eq!(focus_in(&home, "1a-4"), None);
        set_in(&home, "1a-3", None).unwrap();
        set_in(&home, "1a-3", None).unwrap();
        assert_eq!(focus_in(&home, "1a-3"), None);
        assert!(set_in(&home, "../x", Some("no")).is_err());
        let _ = std::fs::remove_dir_all(&home);
    }

    /// An agent may write the folder, so what it leaves there must not hold
    /// the window that reads it.
    #[test]
    fn only_a_plain_small_focus_file_is_read() {
        let home = std::env::temp_dir().join(format!("sonny-zen-odd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let dir = home.join(".agent/sonny/zen");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("aim"), "the receipts").unwrap();
        std::os::unix::fs::symlink(dir.join("aim"), dir.join("link")).unwrap();
        assert_eq!(focus_in(&home, "link"), None);
        std::os::unix::fs::symlink("/dev/zero", dir.join("zero")).unwrap();
        assert_eq!(focus_in(&home, "zero"), None);
        let fifo =
            std::ffi::CString::new(dir.join("fifo").into_os_string().into_encoded_bytes()).unwrap();
        // SAFETY: `fifo` is a valid NUL-terminated path for the call.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert_eq!(focus_in(&home, "fifo"), None);
        std::fs::write(dir.join("big"), "x".repeat(MAX_FOCUS + 1)).unwrap();
        assert_eq!(focus_in(&home, "big"), None);
        std::fs::write(dir.join("full"), "x".repeat(MAX_FOCUS)).unwrap();
        assert_eq!(focus_in(&home, "full").map(|f| f.len()), Some(MAX_FOCUS));
        assert_eq!(focus_in(&home, "aim").as_deref(), Some("the receipts"));
        let _ = std::fs::remove_dir_all(&home);
    }
}
