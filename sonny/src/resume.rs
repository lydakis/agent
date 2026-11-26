//! Program resume (OSC 7577,
//! superlogical.com/rex/docs/build/program-resume): a terminal that gives
//! its programs `OSC7577_TOKEN` keeps the command each one says brings it
//! back, and runs it again after the terminal or its server restarts. Rex
//! does. Sonny says the command that opens this window's agent again, the
//! one it gives Herdr and cmux, each time a window opens on an agent, and
//! takes it back when the person quits. Nothing is said without the token.
//!
//! The token is only for signing: Sonny takes it out of its own
//! environment as it starts, so nothing it starts (the daemon, the tool
//! host, agents' commands) can sign a command of its own.
use std::{
    io::Write,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use base64::Engine;
use sha2::{Digest, Sha256};

const VAR: &str = "OSC7577_TOKEN";

/// The longest report every terminal takes, the whole sequence counted.
const FITS: usize = 2048;

/// The most `argv` elements every terminal takes.
const MOST_ARGS: usize = 64;

/// The terminal's token, taken from the environment as Sonny started.
static TOKEN: Mutex<Option<String>> = Mutex::new(None);

/// Whether this process said a command, so quitting takes it back.
static SAID: AtomicBool = AtomicBool::new(false);

/// Keep the terminal's token, and take it out of the environment, so no
/// program Sonny starts gets it. First thing in `main`, while Sonny is
/// still one thread.
pub fn at_start() {
    let Some(token) = std::env::var_os(VAR) else {
        return;
    };
    // SAFETY: called first in `main`, before any other thread exists.
    unsafe { std::env::remove_var(VAR) };
    let token = token.into_string().ok().filter(|t| valid_token(t));
    *TOKEN.lock().unwrap_or_else(|e| e.into_inner()) = token;
}

/// The token, for a Sonny this process becomes (`exec` on an upgrade),
/// which is this same program going on.
pub fn token() -> Option<(&'static str, String)> {
    TOKEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .map(|t| (VAR, t))
}

/// 22 to 64 of `[A-Za-z0-9_-]`, as the protocol has it.
fn valid_token(token: &str) -> bool {
    (22..=64).contains(&token.len())
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// This window's agent comes back with `argv`, run in `cwd`. A command the
/// protocol cannot carry takes back the one said before instead, so an
/// older one is not run in its place.
pub fn report(argv: &[String], cwd: &std::path::Path) {
    let Some(token) = token().map(|(_, t)| t) else {
        return;
    };
    let (pid, time) = (std::process::id(), now());
    let cwd = cwd.to_str();
    let said = match cwd.and_then(|cwd| body(argv, cwd)) {
        Some(body) => {
            let sequence = sequence(&token, pid, time, &body);
            // One too long for some terminals: the old one is taken back
            // first, so a terminal that drops this one runs neither.
            if sequence.len() > FITS {
                write(&sequence_clear(&token, pid, time));
            }
            write(&sequence);
            true
        }
        None => {
            write(&sequence_clear(&token, pid, time));
            false
        }
    };
    SAID.store(said, Ordering::Relaxed);
}

/// The person quit: the window is their shell again, and nothing comes
/// back in it. Never on a hangup or a closed terminal, which end Sonny
/// before it gets here.
pub fn clear() {
    if !SAID.swap(false, Ordering::Relaxed) {
        return;
    }
    if let Some(token) = token().map(|(_, t)| t) {
        write(&sequence_clear(&token, std::process::id(), now()));
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn write(sequence: &str) {
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(sequence.as_bytes());
    let _ = stdout.flush();
}

/// `argv=…` (and `cwd=…`), or none when a part cannot be carried: no
/// program, too many arguments, or a control character in any.
fn body(argv: &[String], cwd: &str) -> Option<String> {
    let carried = |text: &str| !text.chars().any(char::is_control);
    let first = argv.first()?;
    if first.is_empty()
        || argv.len() > MOST_ARGS
        || !argv.iter().all(|a| carried(a))
        || !cwd.starts_with('/')
        || !carried(cwd)
    {
        return None;
    }
    let b64 = |text: &str| base64::engine::general_purpose::STANDARD.encode(text);
    let argv: Vec<String> = argv.iter().map(|a| b64(a)).collect();
    Some(format!("argv={}:cwd={}", argv.join(","), b64(cwd)))
}

/// The whole report for `body`, signed with `token`.
fn sequence(token: &str, pid: u32, time: u64, body: &str) -> String {
    let signed = format!("pid={pid}:time={time}:{body}");
    let mac = mac(&signed, token);
    format!("\x1b]7577;{signed}:mac={mac}\x1b\\")
}

fn sequence_clear(token: &str, pid: u32, time: u64) -> String {
    sequence(token, pid, time, "clear=1")
}

/// SHA-256 of the signed pairs, `:`, and the token, in lowercase hex.
fn mac(signed: &str, token: &str) -> String {
    let digest = Sha256::digest(format!("{signed}:{token}").as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The protocol's own example: Neovim taking its command back.
    #[test]
    fn signs_as_the_protocol_does() {
        let clear = sequence_clear("dummy-token-replace-me", 74877, 1795543200);
        assert!(
            clear.starts_with("\x1b]7577;pid=74877:time=1795543200:clear=1:mac=c30603369f98"),
            "{clear:?}"
        );
        assert!(clear.ends_with("\x1b\\"));
        let mac = clear
            .rsplit_once("mac=")
            .unwrap()
            .1
            .trim_end_matches("\x1b\\");
        assert_eq!(mac.len(), 64);
        assert!(
            mac.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
    }

    #[test]
    fn carries_the_command_and_its_folder() {
        let argv = ["sonny", "--lead", "fix"].map(String::from);
        let body = body(&argv, "/home/user/src/project").unwrap();
        assert_eq!(
            body,
            "argv=c29ubnk=,LS1sZWFk,Zml4:cwd=L2hvbWUvdXNlci9zcmMvcHJvamVjdA=="
        );
        let report = sequence("dummy-token-replace-me", 1, 2, &body);
        assert!(report.starts_with("\x1b]7577;pid=1:time=2:argv=c29ubnk="));
        // Nothing in it but what a value may hold, and the token never.
        let inner = &report[7..report.len() - 2];
        assert!(!inner.contains("dummy-token"));
        assert!(
            inner
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.,+/=-:".contains(&b))
        );
    }

    #[test]
    fn what_it_cannot_carry_is_not_said() {
        let line = ["sonny", "a\nb"].map(String::from);
        assert_eq!(body(&line, "/tmp"), None);
        assert_eq!(body(&[String::new()], "/tmp"), None);
        assert_eq!(body(&["sonny".into()], "relative"), None);
        let many: Vec<String> = (0..65).map(|i| i.to_string()).collect();
        assert_eq!(body(&many, "/tmp"), None);
    }

    #[test]
    fn takes_only_a_token_the_protocol_allows() {
        assert!(valid_token("dummy-token-replace-me"));
        assert!(!valid_token("short"));
        assert!(!valid_token("has spaces in it, twenty-two+"));
        assert!(!valid_token(&"a".repeat(65)));
    }
}
