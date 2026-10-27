//! The environment an interactive login shell ends up with, where people
//! export provider keys: what a daemon started from launchd, which gives
//! only `HOME`, `SHELL` and `PATH`, needs to reach a provider.
use std::ffi::OsString;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::Duration;

const SHELL_TIMEOUT: Duration = Duration::from_secs(10);
const MARKER: &str = "__sonny_login_environment__";
const MAX_SHELL_OUTPUT: u64 = 1024 * 1024;

/// `$SHELL -l -i`'s environment, bounded in time and in bytes: a profile
/// that hangs or prints without end is no environment.
pub fn environment() -> Option<Vec<(OsString, OsString)>> {
    let shell = std::env::var_os("SHELL").filter(|shell| !shell.is_empty())?;
    let mut child = Command::new(shell)
        .args(["-l", "-i", "-c"])
        .arg(format!("echo {MARKER}; /usr/bin/env -0"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let (sent, read) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut output = Vec::new();
        let ok = stdout
            .take(MAX_SHELL_OUTPUT + 1)
            .read_to_end(&mut output)
            .is_ok();
        let _ = sent.send(ok.then_some(output));
    });
    let output = read.recv_timeout(SHELL_TIMEOUT).ok().flatten();
    let Some(output) = output else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    if output.len() as u64 > MAX_SHELL_OUTPUT {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }
    if !child.wait().ok()?.success() {
        return None;
    }
    parse(&output)
}

/// The `KEY=VALUE` entries after the marker, whatever the profile printed
/// before it; a value may hold anything, the marker included.
fn parse(stdout: &[u8]) -> Option<Vec<(OsString, OsString)>> {
    use std::os::unix::ffi::OsStrExt;
    let marker = format!("{MARKER}\n");
    let start = stdout
        .windows(marker.len())
        .position(|window| window == marker.as_bytes())?
        + marker.len();
    let environment: Vec<_> = stdout[start..]
        .split(|byte| *byte == 0)
        .filter_map(|entry| {
            let at = entry.iter().position(|byte| *byte == b'=')?;
            (at > 0).then(|| {
                (
                    std::ffi::OsStr::from_bytes(&entry[..at]).to_owned(),
                    std::ffi::OsStr::from_bytes(&entry[at + 1..]).to_owned(),
                )
            })
        })
        .collect();
    (!environment.is_empty()).then_some(environment)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_environment_is_read_after_whatever_the_profile_printed() {
        let stdout = format!("Welcome!\n{MARKER}\nHOME=/h\0KEY=a=b\n{MARKER}\n\0=bad\0junk\0");
        let environment = parse(stdout.as_bytes()).unwrap();
        assert_eq!(
            environment,
            vec![
                ("HOME".into(), "/h".into()),
                ("KEY".into(), format!("a=b\n{MARKER}\n").into())
            ]
        );
        assert!(parse(b"no marker\0HOME=/h\0").is_none());
        assert!(parse(format!("{MARKER}\n").as_bytes()).is_none());
    }
}
