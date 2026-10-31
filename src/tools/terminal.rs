//! Terminals: a command run in a pseudo-terminal that its bot types into
//! with `write_stdin` and reads back while it runs, as Codex's unified exec
//! runs one, for a REPL or a program that prompts. A terminal lives until
//! its command exits or is killed, across turns, and reports its end on its
//! proc handle as a background command does.
use super::{Credentials, DRAIN_PAUSE, MAX_YIELD_MS, Outcome, PREVIEW_BYTES, Registry, sh};
use crate::{Error, Result, fail_with, sandbox::Policy};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::unix::AsyncFd,
    sync::{oneshot, watch},
    time::Instant,
};

/// Terminals one bot keeps running; a new one past this ends the one it
/// used least recently, as Codex prunes its sessions.
pub const MAX_TERMINALS: usize = 16;
/// How long a write waits for output by default: Codex's waits.
const WRITE_YIELD_MS: u64 = 250;
const POLL_YIELD_MS: u64 = 5_000;
/// How long typed input may wait for a program that reads none.
const INPUT_WAIT: Duration = Duration::from_secs(5);
/// The terminal's size: wide enough that few programs wrap their lines.
const COLUMNS: u16 = 120;
const ROWS: u16 = 40;

pub type Terminals = Arc<Mutex<HashMap<i64, Arc<Terminal>>>>;

pub struct Terminal {
    /// The bot's id, which a later bot of the same name does not share.
    owner: i64,
    /// What confines the command, which input typed later stays under.
    sandbox: Option<Arc<Policy>>,
    master: AsyncFd<OwnedFd>,
    /// The command's process group, which its session leads.
    group: i32,
    state: Mutex<State>,
    exited: watch::Receiver<bool>,
}

struct State {
    /// Output not yet read: at most its last `PREVIEW_BYTES`.
    unread: Vec<u8>,
    dropped: u64,
    /// How the command ended, with what it wrote that no write read.
    end: Option<(Value, String)>,
    used: Instant,
}

impl Terminal {
    /// End the command's whole session: its own group, the job in the
    /// terminal's foreground, and, looked up off the runtime thread, every
    /// other group a shell with job control made in it.
    fn kill(&self) {
        // Only while the command runs: after its end the group's number
        // may be another's.
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.end.is_none() {
            // SAFETY: kill and tcgetpgrp take no pointers; the group and
            // the terminal are this command's own.
            unsafe {
                libc::kill(-self.group, libc::SIGKILL);
                let foreground = libc::tcgetpgrp(self.master.get_ref().as_raw_fd());
                if foreground > 0 {
                    libc::kill(-foreground, libc::SIGKILL);
                }
            }
            end_session(self.group);
        }
    }

    async fn write(&self, mut input: &[u8]) -> Result<()> {
        let typed = async {
            while !input.is_empty() {
                let mut ready = self.master.writable().await?;
                match ready.try_io(|fd| {
                    // SAFETY: the pointer and length are the slice's own.
                    let n =
                        unsafe { libc::write(fd.as_raw_fd(), input.as_ptr().cast(), input.len()) };
                    match n {
                        n if n < 0 => Err(std::io::Error::last_os_error()),
                        n => Ok(n as usize),
                    }
                }) {
                    Ok(written) => input = &input[written?..],
                    Err(_) => continue,
                }
            }
            Ok::<_, Error>(())
        };
        tokio::time::timeout(INPUT_WAIT, typed).await.map_err(|_| {
            Error::with(
                "terminal_input_blocked",
                "the program read none of the input for 5 s",
            )
        })?
    }

    /// What the command wrote since the last read, and how it ended once
    /// it has.
    fn take(&self, registry: &Registry, handle: i64) -> Outcome {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.used = Instant::now();
        let dropped = std::mem::take(&mut state.dropped);
        let (text, end) = match &mut state.end {
            Some((end, rest)) => (std::mem::take(rest), Some(end.clone())),
            None => (text(&mut state.unread, Some(&registry.credentials)), None),
        };
        drop(state);
        report(registry, Some(handle), text, dropped, end)
    }

    /// Wait up to `yield_ms` for the command to end.
    async fn settle(&self, yield_ms: u64) {
        let mut exited = self.exited.clone();
        let _ = tokio::time::timeout(
            Duration::from_millis(yield_ms),
            exited.wait_for(|exited| *exited),
        )
        .await;
    }
}

/// `owner`'s least recently used terminal, when it has `share` or more.
/// The scan is bounded: every terminal holds a lasting permit.
fn least_used(
    terminals: &HashMap<i64, Arc<Terminal>>,
    owner: i64,
    share: usize,
) -> Option<Arc<Terminal>> {
    let own = terminals.values().filter(|t| t.owner == owner);
    if own.clone().count() < share {
        return None;
    }
    own.min_by_key(|t| t.state.lock().unwrap_or_else(|e| e.into_inner()).used)
        .cloned()
}

/// A start's, a write's, or an end's result: the output, and how the
/// command ended once it has, or that it runs.
fn report(
    registry: &Registry,
    handle: Option<i64>,
    text: String,
    dropped: u64,
    end: Option<Value>,
) -> Outcome {
    let mut output = json!({"output":registry.redact(text)});
    if let Some(handle) = handle {
        output["handle"] = format!("proc:{handle}").into();
    }
    if dropped > 0 {
        output["dropped_bytes"] = dropped.into();
    }
    let failed = match end {
        Some(Value::Object(end)) => {
            let failed = end.get("exit_code") != Some(&json!(0));
            output.as_object_mut().expect("an object").extend(end);
            failed
        }
        _ => {
            output["running"] = true.into();
            false
        }
    };
    Outcome {
        failed,
        ..Outcome::text(output.to_string())
    }
}

/// Decode what was read, keeping for the next read a character split
/// across reads and, while the command runs, the carriage returns and the
/// start of a credential that more output may finish; a terminal's line
/// ends read as plain newlines.
fn text(unread: &mut Vec<u8>, running: Option<&Credentials>) -> String {
    let mut whole = match std::str::from_utf8(unread) {
        Err(error) if error.error_len().is_none() => error.valid_up_to(),
        _ => unread.len(),
    };
    if let Some(credentials) = running {
        while whole > 0 && unread[whole - 1] == b'\r' {
            whole -= 1;
        }
        whole -= credentials.unfinished(&unread[..whole]);
    }
    let rest = unread.split_off(whole);
    let read = std::mem::replace(unread, rest);
    lines(&String::from_utf8_lossy(&read))
}

/// `text` with each run of carriage returns before a newline dropped: a
/// program's own carriage return before the terminal's makes two.
fn lines(text: &str) -> String {
    let mut lines = String::with_capacity(text.len());
    let mut returns = 0;
    for c in text.chars() {
        match c {
            '\r' => returns += 1,
            '\n' => {
                returns = 0;
                lines.push('\n');
            }
            c => {
                lines.push_str(&"\r".repeat(returns));
                returns = 0;
                lines.push(c);
            }
        }
    }
    lines.push_str(&"\r".repeat(returns));
    lines
}

/// Kill every process group in session `session` but its leader's, which
/// the caller ends: the jobs a shell with job control starts each get a
/// group of their own, and one that ignores SIGHUP would outlive its
/// terminal. A session's number stays its own while any member lives, so
/// only a session already empty could find its number reused. The listing
/// reads files, so it runs off the runtime thread when there is one.
fn end_session(session: i32) {
    match tokio::runtime::Handle::try_current() {
        Ok(runtime) => drop(runtime.spawn_blocking(move || sweep(session))),
        Err(_) => sweep(session),
    }
}

fn sweep(session: i32) {
    for group in session_groups(session) {
        if group != session {
            // SAFETY: kill takes no pointers.
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
        }
    }
}

/// The process groups with a member in `session`.
#[cfg(target_os = "linux")]
fn session_groups(session: i32) -> Vec<i32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut groups = Vec::new();
    for entry in entries.flatten() {
        if !entry
            .file_name()
            .as_encoded_bytes()
            .iter()
            .all(u8::is_ascii_digit)
        {
            continue;
        }
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // After the command's name, which may hold spaces: state, parent,
        // group, session.
        let Some((_, fields)) = stat.rsplit_once(')') else {
            continue;
        };
        let mut fields = fields.split_whitespace().skip(2);
        if let (Some(Ok(group)), Some(Ok(sid))) = (
            fields.next().map(str::parse::<i32>),
            fields.next().map(str::parse::<i32>),
        ) && sid == session
            && !groups.contains(&group)
        {
            groups.push(group);
        }
    }
    groups
}

/// The process groups with a member in `session`.
#[cfg(target_os = "macos")]
fn session_groups(session: i32) -> Vec<i32> {
    // SAFETY: a null buffer asks only for the count.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if count <= 0 {
        return Vec::new();
    }
    // Room for processes started since the count.
    let mut pids = vec![0 as libc::pid_t; count as usize + 64];
    // SAFETY: the pointer and size are the buffer's own.
    let listed = unsafe {
        libc::proc_listallpids(
            pids.as_mut_ptr().cast(),
            (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int,
        )
    };
    pids.truncate(listed.max(0) as usize);
    let mut groups = Vec::new();
    for pid in pids {
        // SAFETY: getsid and getpgid take no pointers.
        let (sid, group) = unsafe { (libc::getsid(pid), libc::getpgid(pid)) };
        if sid == session && group > 0 && !groups.contains(&group) {
            groups.push(group);
        }
    }
    groups
}

/// Elsewhere only the leader's group and the foreground job are ended.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn session_groups(_session: i32) -> Vec<i32> {
    Vec::new()
}

/// A pseudo-terminal's two ends, each closed on exec.
fn open() -> Result<(OwnedFd, OwnedFd)> {
    let (mut master, mut slave) = (-1, -1);
    let mut size = libc::winsize {
        ws_row: ROWS,
        ws_col: COLUMNS,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: the out pointers are valid; no name or settings are asked.
    // The size is a pointer of either mutability, as platforms declare it.
    if unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::addr_of_mut!(size),
        )
    } < 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: openpty returned these two descriptors, owned by no one else.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    for fd in [&master, &slave] {
        // SAFETY: fcntl on a descriptor this function owns.
        unsafe {
            libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
        }
    }
    // SAFETY: as above; the master is read without blocking.
    unsafe {
        let flags = libc::fcntl(master.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    Ok((master, slave))
}

impl Registry {
    /// Start `command` in a terminal of `owner`'s, recorded as proc
    /// `handle`, and return what it writes within `yield_ms`. Its end goes
    /// to `done`, as a background command's result does.
    #[allow(clippy::too_many_arguments)]
    pub async fn terminal(
        &self,
        owner: i64,
        handle: i64,
        command: String,
        workspace: PathBuf,
        yield_ms: u64,
        environment: Vec<(String, String)>,
        sandbox: Option<Arc<Policy>>,
        done: oneshot::Sender<Result<Outcome>>,
    ) -> Result<Outcome> {
        let started = async {
            // The bot's least recently used terminal past its share ends
            // first, so its permit is free for this one.
            self.prune(owner).await;
            let permit = self.lasting()?;
            let held = crate::sandbox::hold_for(sandbox.as_deref()).await;
            let (master, slave) = open()?;
            let mut process = sh(
                &command,
                &workspace,
                self,
                &environment,
                sandbox.as_deref(),
                false,
            )?;
            process
                .stdin(slave.try_clone()?)
                .stdout(slave.try_clone()?)
                .stderr(slave)
                .kill_on_drop(true);
            // SAFETY: setsid and ioctl are async-signal-safe; the terminal
            // becomes the new session's own.
            unsafe {
                process.pre_exec(|| {
                    if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let child = process.spawn()?;
            // The parent's copies of the terminal's far end close here, so
            // the master reads its end once the command's do.
            drop(process);
            let group = child.id().ok_or(Error::new("shell_spawn_failed"))? as i32;
            // SAFETY: the OwnedFd is open, and it keeps the one descriptor
            // until the AsyncFd that owns it drops.
            let master = unsafe { AsyncFd::register(master) }.map_err(std::io::Error::from)?;
            Ok::<_, Error>((child, master, group, (held, permit)))
        };
        let (mut child, master, group, held) = match started.await {
            Ok(started) => started,
            Err(error) => {
                let _ = done.send(Err(error.clone()));
                return Err(error);
            }
        };
        let (exit, exited) = watch::channel(false);
        let terminal = Arc::new(Terminal {
            owner,
            sandbox,
            master,
            group,
            state: Mutex::new(State {
                unread: Vec::new(),
                dropped: 0,
                end: None,
                used: Instant::now(),
            }),
            exited,
        });
        // Two starts at once can both find room: the later one's insert
        // ends the bot's least recently used, as before.
        let pruned = {
            let mut terminals = self.terminals.lock().unwrap_or_else(|e| e.into_inner());
            let pruned = least_used(&terminals, owner, MAX_TERMINALS);
            terminals.insert(handle, terminal.clone());
            pruned
        };
        if let Some(pruned) = pruned {
            pruned.kill();
        }
        let reader = {
            let terminal = terminal.clone();
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 16 * 1024];
                loop {
                    let Ok(mut ready) = terminal.master.readable().await else {
                        return;
                    };
                    let read = ready.try_io(|fd| {
                        // SAFETY: the pointer and length are the buffer's own.
                        let n = unsafe {
                            libc::read(fd.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len())
                        };
                        match n {
                            n if n < 0 => Err(std::io::Error::last_os_error()),
                            n => Ok(n as usize),
                        }
                    });
                    match read {
                        Err(_) => continue,
                        // An end of file, or the far end gone (EIO).
                        Ok(Ok(0) | Err(_)) => return,
                        Ok(Ok(n)) => {
                            let dropping = {
                                let mut state =
                                    terminal.state.lock().unwrap_or_else(|e| e.into_inner());
                                state.unread.extend_from_slice(&buffer[..n]);
                                let over = state.unread.len().saturating_sub(PREVIEW_BYTES);
                                if over > 0 {
                                    state.unread.drain(..over);
                                    state.dropped += over as u64;
                                }
                                over > 0
                            };
                            // What no read keeps is drained slowly, as a
                            // command's pipe is: the writer waits on the
                            // terminal instead of spending the daemon's thread.
                            if dropping {
                                tokio::time::sleep(DRAIN_PAUSE).await;
                            }
                        }
                    }
                }
            })
        };
        {
            let (registry, terminal) = (self.clone(), terminal.clone());
            tokio::spawn(async move {
                let status = child.wait().await;
                // What the command wrote last is still on its way; a process
                // it left running may hold the terminal open, so this waits
                // only a moment before the rest of its group is killed. One
                // in another session outlives that, so the reader stops
                // here and lets the terminal go.
                let mut reader = reader;
                if tokio::time::timeout(Duration::from_millis(100), &mut reader)
                    .await
                    .is_err()
                {
                    reader.abort();
                    let _ = reader.await;
                }
                // SAFETY: kill takes no pointers; the group is this command's own.
                unsafe {
                    libc::kill(-group, libc::SIGKILL);
                }
                // The jobs it started in groups of their own go too, before
                // its permit does.
                let _ = tokio::task::spawn_blocking(move || sweep(group)).await;
                drop(held);
                let end = match status {
                    Ok(status) => {
                        use std::os::unix::process::ExitStatusExt;
                        match status.signal() {
                            Some(signal) => json!({"exit_code":null,"signal":signal}),
                            None => json!({"exit_code":status.code()}),
                        }
                    }
                    Err(error) => json!({"exit_code":null,"error":error.to_string()}),
                };
                // A wait on the handle reports the end, with what no write
                // read; a write that sees the end reports that too.
                let last = {
                    let mut state = terminal.state.lock().unwrap_or_else(|e| e.into_inner());
                    let rest = text(&mut state.unread, None);
                    let last = report(
                        &registry,
                        None,
                        rest.clone(),
                        state.dropped,
                        Some(end.clone()),
                    );
                    state.end = Some((end, rest));
                    last
                };
                registry
                    .terminals
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&handle);
                let _ = exit.send(true);
                let _ = done.send(Ok(last));
            });
        }
        terminal.settle(yield_ms).await;
        Ok(terminal.take(self, handle))
    }

    /// End `owner`'s least recently used terminal when it already runs its
    /// share, and wait a moment for it to go, so its permit is free.
    async fn prune(&self, owner: i64) {
        let pruned = least_used(
            &self.terminals.lock().unwrap_or_else(|e| e.into_inner()),
            owner,
            MAX_TERMINALS,
        );
        if let Some(pruned) = pruned {
            pruned.kill();
            let mut exited = pruned.exited.clone();
            let _ = tokio::time::timeout(Duration::from_secs(2), exited.wait_for(|e| *e)).await;
        }
    }

    /// End every terminal, as the daemon stops; each reports its end on
    /// its handle.
    pub fn end_terminals(&self) {
        let terminals: Vec<Arc<Terminal>> = self
            .terminals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        for terminal in terminals {
            terminal.kill();
        }
    }

    /// Type `input` into `owner`'s terminal `handle`, or kill it, and return
    /// what it writes within `yield_ms`. A terminal whose sandbox is no
    /// longer its bot's, `sandbox`, takes no more input: it is ended.
    pub async fn write_stdin(
        &self,
        owner: i64,
        handle: i64,
        input: &str,
        yield_ms: Option<u64>,
        kill: bool,
        sandbox: Option<Arc<Policy>>,
    ) -> Result<Outcome> {
        let terminal = self
            .terminals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&handle)
            .filter(|terminal| terminal.owner == owner)
            .cloned();
        let Some(terminal) = terminal else {
            return fail_with(
                "terminal_not_running",
                format!(
                    "proc:{handle} is no terminal of this agent's that still runs; wait on it for how it ended"
                ),
            );
        };
        if terminal.sandbox != sandbox {
            terminal.kill();
            return fail_with(
                "terminal_sandbox_changed",
                format!(
                    "this agent's sandbox changed since proc:{handle} started, so it was ended; start it again"
                ),
            );
        }
        let yield_ms = if kill {
            terminal.kill();
            MAX_YIELD_MS
        } else if input.is_empty() {
            yield_ms.unwrap_or(POLL_YIELD_MS)
        } else {
            terminal.write(input.as_bytes()).await?;
            yield_ms.unwrap_or(WRITE_YIELD_MS)
        };
        terminal.settle(yield_ms).await;
        Ok(terminal.take(self, handle))
    }
}

#[cfg(test)]
mod tests {
    use super::{Credentials, lines, text};

    #[test]
    fn a_character_split_across_reads_waits_for_its_end() {
        let none = Credentials::default();
        let mut unread = "a\r\r\né".as_bytes().to_vec();
        let last = unread.pop().unwrap();
        assert_eq!(text(&mut unread, Some(&none)), "a\n");
        unread.push(last);
        assert_eq!(text(&mut unread, Some(&none)), "é");
        assert!(unread.is_empty());
    }

    #[test]
    fn a_credential_split_across_reads_is_redacted_whole() {
        let credentials = Credentials::default();
        credentials.set("KEY", "sk-secret-1234");
        let mut unread = b"key: sk-sec".to_vec();
        assert_eq!(text(&mut unread, Some(&credentials)), "key: ");
        unread.extend_from_slice(b"ret-1234\r\n> ");
        let read = text(&mut unread, Some(&credentials));
        assert_eq!(credentials.redact(read), "[REDACTED]\n> ");
        // A whole credential that ends with its own start is not split.
        credentials.set("OTHER", "sk-example-s");
        unread.extend_from_slice(b"sk-example-s");
        assert_eq!(
            credentials.redact(text(&mut unread, Some(&credentials))),
            "[REDACTED]"
        );
        assert!(unread.is_empty());
        // A command's end holds nothing back.
        unread.extend_from_slice(b"sk-");
        assert_eq!(text(&mut unread, None), "sk-");
    }

    #[test]
    fn a_secret_field_split_across_reads_is_redacted_whole() {
        let credentials = Credentials::default();
        credentials.redact_also(&["password".to_owned(), "api_key".to_owned()], &[]);
        let read = |unread: &mut Vec<u8>, more: &[u8]| {
            unread.extend_from_slice(more);
            credentials.redact(text(unread, Some(&credentials)))
        };
        let mut unread = Vec::new();
        // The name and separator wait for their value, and the value for
        // its end.
        assert_eq!(read(&mut unread, b"login ok\npassword="), "login ok\n");
        assert_eq!(read(&mut unread, b"fake9fake9"), "");
        assert_eq!(read(&mut unread, b"fake9\n> "), "password=[REDACTED]\n> ");
        // So does a name cut inside, from four bytes of a field's start.
        assert_eq!(read(&mut unread, b"export MY_PASS"), "export ");
        assert_eq!(
            read(&mut unread, b"WORD=abcd1234efgh\n"),
            "MY_PASSWORD=[REDACTED]\n"
        );
        // Other words, and fields whose values ended, go out as typed.
        assert_eq!(read(&mut unread, b"ls -la\n> "), "ls -la\n> ");
        assert_eq!(read(&mut unread, b"api_key: set\n"), "api_key: set\n");
        assert!(unread.is_empty());
        // A command's end holds nothing back.
        unread.extend_from_slice(b"password=");
        assert_eq!(text(&mut unread, None), "password=");
    }

    #[test]
    fn carriage_returns_before_a_newline_go_in_one_pass() {
        let many = format!("{}\n", "\r".repeat(60_000));
        assert_eq!(lines(&many), "\n");
        assert_eq!(lines("a\rb\r\r\nc\r"), "a\rb\nc\r");
    }
}
