//! The environment an interactive login shell ends up with, where people
//! export provider keys: what a daemon started from launchd, which gives
//! only `HOME`, `SHELL` and `PATH`, needs to reach a provider.
use std::ffi::OsString;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const SHELL_TIMEOUT: Duration = Duration::from_secs(10);
const MARKER: &str = "__sonny_login_environment__";
const MAX_SHELL_OUTPUT: u64 = 1024 * 1024;

/// `$SHELL -l -i`'s environment, bounded in time and in bytes: a profile
/// that hangs or prints without end is no environment.
pub fn environment() -> Option<Vec<(OsString, OsString)>> {
    let shell = std::env::var_os("SHELL").filter(|shell| !shell.is_empty())?;
    read(&shell, SHELL_TIMEOUT)
}

fn read(shell: &std::ffi::OsStr, timeout: Duration) -> Option<Vec<(OsString, OsString)>> {
    let mut child = Command::new(shell)
        .args(["-l", "-i", "-c"])
        .arg(format!("echo {MARKER}; /usr/bin/env -0"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let (sent, output) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut output = Vec::new();
        let ok = stdout
            .take(MAX_SHELL_OUTPUT + 1)
            .read_to_end(&mut output)
            .is_ok();
        let _ = sent.send(ok.then_some(output));
    });
    // One deadline for the output and the exit: a shell that closes its
    // output and then hangs is stopped all the same.
    let deadline = Instant::now() + timeout;
    let output = output.recv_timeout(timeout).ok().flatten();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => break None,
        }
    };
    let Some(status) = status else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    let output = output?;
    if !status.success() || output.len() as u64 > MAX_SHELL_OUTPUT {
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

    #[test]
    fn a_shell_that_hangs_after_closing_its_output_is_stopped() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("sonny-login-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let shell = dir.join("shell");
        std::fs::write(
            &shell,
            "#!/bin/sh\nsh -c \"$4\"\nexec >/dev/null\nexec sleep 30\n",
        )
        .unwrap();
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
        let started = Instant::now();
        assert!(read(shell.as_os_str(), Duration::from_millis(300)).is_none());
        assert!(started.elapsed() < Duration::from_secs(5));
        // The same shell without the hang gives its environment.
        std::fs::write(&shell, "#!/bin/sh\nKEY=v sh -c \"$4\"\n").unwrap();
        let environment = read(shell.as_os_str(), Duration::from_secs(10)).unwrap();
        assert!(environment.contains(&("KEY".into(), "v".into())));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
