//! Programs started responsible for themselves. macOS grants a permission
//! such as Accessibility or Screen Recording to the process it holds
//! responsible, which is not the one asking: a program started from a
//! terminal is the terminal's, and so is everything under it, Sonny's
//! daemon and tool host included when one was started there. So a grant
//! to agent-desktop itself, the one its guide asks for, would never count,
//! and which app needs it would depend on where the daemon happened to
//! start. Started with its responsibility disclaimed, as Chromium, LLDB
//! and terminals start their children, a program is its own: what is
//! granted to it is what it is allowed. Elsewhere no permission follows a
//! parent, and it starts as any other.
use std::ffi::OsStr;
use std::path::Path;

/// What a program printed, and whether it exited with success.
pub struct Output {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub success: bool,
}

/// Run `program` with `args` in `dir`, with no input, without the
/// variables `removed`, to its end; it is killed if this is dropped first.
pub async fn output(
    program: &Path,
    args: &[String],
    dir: &Path,
    removed: &[&str],
) -> std::io::Result<Output> {
    #[cfg(target_os = "macos")]
    {
        own::output(program, args, dir, removed).await
    }
    #[cfg(not(target_os = "macos"))]
    {
        let mut command = tokio::process::Command::new(program);
        command
            .args(args)
            .current_dir(dir)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        for name in removed {
            command.env_remove(name);
        }
        let out = command.output().await?;
        Ok(Output {
            stdout: out.stdout,
            stderr: out.stderr,
            success: out.status.success(),
        })
    }
}

/// `name=value` for each of this process's variables but `removed`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn environment(removed: &[&str]) -> Vec<std::ffi::OsString> {
    std::env::vars_os()
        .filter(|(name, _)| !removed.iter().any(|r| OsStr::new(r) == name))
        .map(|(name, value)| {
            let mut pair = name;
            pair.push("=");
            pair.push(value);
            pair
        })
        .collect()
}

#[cfg(target_os = "macos")]
mod own {
    use super::Output;
    use std::ffi::{CString, OsString};
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    unsafe extern "C" {
        // libSystem's, declared in its spawn_private.h.
        fn responsibility_spawnattrs_setdisclaim(
            attrs: *mut libc::posix_spawnattr_t,
            disclaim: libc::c_int,
        ) -> libc::c_int;
    }

    fn check(code: libc::c_int) -> std::io::Result<()> {
        match code {
            0 => Ok(()),
            code => Err(std::io::Error::from_raw_os_error(code)),
        }
    }

    fn c(bytes: &[u8]) -> std::io::Result<CString> {
        CString::new(bytes).map_err(|_| std::io::Error::other("a NUL byte in an argument"))
    }

    /// A pipe whose ends close in any program started after it, but where
    /// the start puts one in place.
    fn pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
        let mut fds = [0; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let ends = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        for end in [&ends.0, &ends.1] {
            if unsafe { libc::fcntl(end.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(ends)
    }

    /// The program, started; `None` once it has been waited for.
    struct Started(Arc<Mutex<Option<libc::pid_t>>>);

    impl Drop for Started {
        /// Killed if not yet ended: until it is waited for, its ID is its
        /// own, so no other process gets the signal.
        fn drop(&mut self) {
            if let Some(pid) = *self.0.lock().unwrap_or_else(|e| e.into_inner()) {
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
    }

    /// Start `program`, responsible for itself, with its output on two
    /// pipes and nothing else of this process's open in it.
    fn spawn(
        program: &Path,
        args: &[String],
        dir: &Path,
        removed: &[&str],
    ) -> std::io::Result<(libc::pid_t, OwnedFd, OwnedFd)> {
        let path = c(program.as_os_str().as_bytes())?;
        let mut argv = vec![path.clone()];
        for arg in args {
            argv.push(c(arg.as_bytes())?);
        }
        let envp: Vec<CString> = (super::environment(removed).iter())
            .map(|pair: &OsString| c(pair.as_bytes()))
            .collect::<Result<_, _>>()?;
        let pointers = |strings: &[CString]| -> Vec<*mut libc::c_char> {
            (strings.iter().map(|s| s.as_ptr() as *mut libc::c_char))
                .chain(std::iter::once(std::ptr::null_mut()))
                .collect()
        };
        let (argv_p, envp_p) = (pointers(&argv), pointers(&envp));
        let null = c(b"/dev/null")?;
        let dir = c(dir.as_os_str().as_bytes())?;
        let (out_r, out_w) = pipe()?;
        let (err_r, err_w) = pipe()?;
        unsafe {
            let mut actions: libc::posix_spawn_file_actions_t = std::ptr::null_mut();
            check(libc::posix_spawn_file_actions_init(&mut actions))?;
            let mut attrs: libc::posix_spawnattr_t = std::ptr::null_mut();
            if let Err(e) = check(libc::posix_spawnattr_init(&mut attrs)) {
                libc::posix_spawn_file_actions_destroy(&mut actions);
                return Err(e);
            }
            let started = (|| -> std::io::Result<libc::pid_t> {
                check(libc::posix_spawn_file_actions_addopen(
                    &mut actions,
                    0,
                    null.as_ptr(),
                    libc::O_RDONLY,
                    0,
                ))?;
                check(libc::posix_spawn_file_actions_adddup2(
                    &mut actions,
                    out_w.as_raw_fd(),
                    1,
                ))?;
                check(libc::posix_spawn_file_actions_adddup2(
                    &mut actions,
                    err_w.as_raw_fd(),
                    2,
                ))?;
                check(libc::posix_spawn_file_actions_addchdir_np(
                    &mut actions,
                    dir.as_ptr(),
                ))?;
                // As a program starts: no signal blocked or ignored, as
                // this one ignores SIGPIPE.
                let mut none: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut none);
                check(libc::posix_spawnattr_setsigmask(&mut attrs, &none))?;
                let mut every: libc::sigset_t = std::mem::zeroed();
                libc::sigfillset(&mut every);
                check(libc::posix_spawnattr_setsigdefault(&mut attrs, &every))?;
                let flags = libc::POSIX_SPAWN_SETSIGMASK
                    | libc::POSIX_SPAWN_SETSIGDEF
                    | libc::POSIX_SPAWN_CLOEXEC_DEFAULT;
                check(libc::posix_spawnattr_setflags(&mut attrs, flags as _))?;
                check(responsibility_spawnattrs_setdisclaim(&mut attrs, 1))?;
                let mut pid: libc::pid_t = 0;
                check(libc::posix_spawn(
                    &mut pid,
                    path.as_ptr(),
                    &actions,
                    &attrs,
                    argv_p.as_ptr(),
                    envp_p.as_ptr(),
                ))?;
                Ok(pid)
            })();
            libc::posix_spawn_file_actions_destroy(&mut actions);
            libc::posix_spawnattr_destroy(&mut attrs);
            Ok((started?, out_r, err_r))
        }
    }

    fn drain(end: OwnedFd) -> tokio::task::JoinHandle<Vec<u8>> {
        tokio::task::spawn_blocking(move || {
            let mut read = Vec::new();
            let _ = std::fs::File::from(end).read_to_end(&mut read);
            read
        })
    }

    pub async fn output(
        program: &Path,
        args: &[String],
        dir: &Path,
        removed: &[&str],
    ) -> std::io::Result<Output> {
        let (pid, out, err) = spawn(program, args, dir, removed)?;
        let started = Started(Arc::new(Mutex::new(Some(pid))));
        let held = started.0.clone();
        let waited = tokio::task::spawn_blocking(move || -> std::io::Result<i32> {
            // Ended but not yet waited for, so its ID stays its own while
            // a drop may still signal it.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            loop {
                let code = unsafe {
                    libc::waitid(
                        libc::P_PID,
                        pid as libc::id_t,
                        &mut info,
                        libc::WEXITED | libc::WNOWAIT,
                    )
                };
                if code == 0 {
                    break;
                }
                let e = std::io::Error::last_os_error();
                if e.kind() != std::io::ErrorKind::Interrupted {
                    return Err(e);
                }
            }
            let mut held = held.lock().unwrap_or_else(|e| e.into_inner());
            let mut status = 0;
            unsafe { libc::waitpid(pid, &mut status, 0) };
            *held = None;
            Ok(status)
        });
        let (stdout, stderr) = (drain(out), drain(err));
        let status = waited.await.map_err(std::io::Error::other)??;
        drop(started);
        Ok(Output {
            stdout: stdout.await.map_err(std::io::Error::other)?,
            stderr: stderr.await.map_err(std::io::Error::other)?,
            success: libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        unsafe extern "C" {
            fn responsibility_get_pid_responsible_for_pid(pid: libc::pid_t) -> libc::pid_t;
        }

        #[tokio::test]
        async fn a_program_started_is_responsible_for_itself_and_says_what_it_printed() {
            // HOME, which every test run has, to see one taken away.
            assert!(std::env::var_os("HOME").is_some());
            let script = "echo $$ $(pwd) ${HOME-unset}; echo said >&2; sleep 1; exit 3";
            let args = vec!["-c".to_owned(), script.to_owned()];
            let ran = tokio::spawn(async move {
                output(Path::new("/bin/sh"), &args, Path::new("/"), &["HOME"]).await
            });
            // Asked while it sleeps, so its ID is still its own.
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            let mut seen = None;
            for line in std::process::Command::new("/bin/ps")
                .args(["-axo", "pid=,ppid=,command="])
                .output()
                .unwrap()
                .stdout
                .split(|b| *b == b'\n')
            {
                let line = String::from_utf8_lossy(line);
                let words: Vec<&str> = line.split_whitespace().collect();
                if words.len() > 2
                    && words[1] == std::process::id().to_string()
                    && line.contains("sleep 1; exit 3")
                {
                    seen = words[0].parse::<libc::pid_t>().ok();
                }
            }
            let pid = seen.expect("the program runs under this one");
            assert_eq!(
                unsafe { responsibility_get_pid_responsible_for_pid(pid) },
                pid
            );
            let ran = ran.await.unwrap().unwrap();
            assert_eq!(
                String::from_utf8_lossy(&ran.stdout),
                format!("{pid} / unset\n")
            );
            assert_eq!(ran.stderr, b"said\n");
            assert!(!ran.success);
        }

        #[tokio::test]
        async fn a_program_dropped_before_its_end_is_killed() {
            let args = ["30".to_owned()];
            let ran = output(Path::new("/bin/sleep"), &args, Path::new("/"), &[]);
            let started = std::time::Instant::now();
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(200), ran)
                    .await
                    .is_err()
            );
            // Its drop killed it: nothing under this process sleeps on.
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            let listed = std::process::Command::new("/bin/ps")
                .args(["-axo", "ppid=,command="])
                .output()
                .unwrap()
                .stdout;
            let me = std::process::id().to_string();
            assert!(
                !String::from_utf8_lossy(&listed)
                    .lines()
                    .any(|line| line.trim_start().starts_with(&format!("{me} "))
                        && line.contains("/bin/sleep 30")),
                "{}",
                String::from_utf8_lossy(&listed)
            );
            assert!(started.elapsed() < std::time::Duration::from_secs(5));
        }
    }
}
