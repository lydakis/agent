//! Helpers for tests that run stand-in programs, and the tests' own home.
use std::{io::Write, path::Path, process::Command};

// Sonny finds nearly everything under `$HOME`: the tests get a fresh one,
// so none reads the person's settings or writes among their things.
agent_client::test_home!();

/// Write `text` to `path` as an executable script, from a child process.
/// A file this process held open for writing would pass to any child
/// another test forks meanwhile, until that child execs, and running the
/// script then fails with "Text file busy".
pub fn script(path: &Path, text: &str) {
    let mut sh = Command::new("sh")
        .args(["-c", r#"cat > "$0" && chmod 755 "$0""#])
        .arg(path)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    sh.stdin.take().unwrap().write_all(text.as_bytes()).unwrap();
    assert!(sh.wait().unwrap().success());
}
