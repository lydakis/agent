//! Hosts reached over SSH. A window opened for a host attaches to the daemon
//! there through one SSH process the app owns for that host: a ControlMaster
//! that also carries the forward of the daemon's Unix socket to a socket in
//! a private local directory. The page's attach, cursor and pulls then run
//! unchanged against a local socket.
//!
//! OpenSSH does everything SSH. The host list is the concrete `Host` aliases
//! of `~/.ssh/config`, what an alias connects to is `ssh -G`'s answer, and
//! every connection is `ssh ALIAS` with the user's own config, keys and agent.
//! No SSH option is read or reimplemented here.
//!
//! The daemon on a host is that host's own `agent`, started with `agent
//! start` in the remote user's login shell, which prints the daemon's ready
//! line and the socket it answered on. The app never copies a binary there
//! and never signals a remote process itself: an older daemon is replaced
//! with the host's own `agent shutdown`, then `agent start`.
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    ffi::OsString,
    path::{Path, PathBuf},
    process::{Output, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::process::{Child, Command};

/// OpenSSH's own bound on reaching the host, in seconds.
const CONNECT_TIMEOUT: u64 = 10;
/// Keepalives, so a link that died without a word is noticed within a minute.
const ALIVE_INTERVAL: u64 = 15;
const ALIVE_COUNT: u64 = 3;
/// Connecting and authenticating, beyond which the master is given up.
const MASTER_TIMEOUT: Duration = Duration::from_secs(30);
/// `agent start` bounds itself at 10 s and `agent shutdown` at 30 s; a
/// login shell and the round trips get the rest.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
/// A failed connection is not tried again before this, doubling from the
/// first to the last, so a window retrying every two seconds does not open
/// an SSH connection each time.
const BACKOFF_FIRST: Duration = Duration::from_secs(1);
const BACKOFF_LAST: Duration = Duration::from_secs(30);
/// What a master's stderr keeps for its reason: the end of it.
const STDERR_KEPT: usize = 4096;
/// Bounds on reading `~/.ssh/config` and what it includes.
const MAX_CONFIG: u64 = 1024 * 1024;
const MAX_INCLUDE_DEPTH: usize = 16;
/// Printed before `agent start`, so the app learns the remote home, the
/// folder a window on the host starts in, whatever a profile prints.
const HOME_MARK: &str = "__agent_app_home__";

/// Whether `alias` is a name to hand `ssh` as its destination: one concrete
/// host, never an option or a pattern.
pub fn valid_alias(alias: &str) -> bool {
    !alias.is_empty()
        && alias.len() <= 255
        && !alias.starts_with('-')
        && !alias.starts_with('!')
        && !alias
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '*' | '?' | '/'))
}

/// The concrete `Host` aliases of an OpenSSH client config, in file order,
/// without patterns (`*`, `?`) or negations (`!`), which are not hosts one
/// can open. `Include` is followed as OpenSSH does for a user config: a
/// relative path from `~/.ssh`, `~/` from home, and wildcards in the last
/// component only; wildcards in a directory are not expanded.
pub fn aliases(config: &Path, home: &Path) -> Vec<String> {
    let mut out = Vec::new();
    read_config(config, home, 0, &mut out);
    out
}

fn read_config(path: &Path, home: &Path, depth: usize, out: &mut Vec<String>) {
    use std::io::Read;
    if depth > MAX_INCLUDE_DEPTH {
        return;
    }
    let mut text = String::new();
    let Ok(file) = std::fs::File::open(path) else {
        return;
    };
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return;
    }
    if file.take(MAX_CONFIG).read_to_string(&mut text).is_err() {
        return;
    }
    for line in text.lines() {
        let words = words(line);
        let Some((keyword, args)) = words.split_first() else {
            continue;
        };
        match keyword.to_ascii_lowercase().as_str() {
            "host" => {
                for alias in args {
                    if valid_alias(alias) && !out.contains(alias) {
                        out.push(alias.clone());
                    }
                }
            }
            "include" => {
                for pattern in args {
                    for included in expand(pattern, home) {
                        read_config(&included, home, depth + 1, out);
                    }
                }
            }
            _ => {}
        }
    }
}

/// A config line's words: the keyword may end at `=`, a double-quoted word
/// keeps its spaces, and a `#` starting a word starts a comment.
fn words(line: &str) -> Vec<String> {
    let line = line.trim();
    let (keyword, rest) = match line.find(|c: char| c.is_whitespace() || c == '=') {
        Some(at) => (&line[..at], line[at..].trim_start()),
        None => (line, ""),
    };
    if keyword.is_empty() || keyword.starts_with('#') {
        return Vec::new();
    }
    let rest = rest.strip_prefix('=').unwrap_or(rest);
    let mut out = vec![keyword.to_owned()];
    let mut chars = rest.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        let Some(first) = chars.next() else { break };
        if first == '#' {
            break;
        }
        let mut word = String::new();
        if first == '"' {
            for c in chars.by_ref() {
                if c == '"' {
                    break;
                }
                word.push(c);
            }
        } else {
            word.push(first);
            while let Some(c) = chars.next_if(|c| !c.is_whitespace()) {
                word.push(c);
            }
        }
        out.push(word);
    }
    out
}

fn expand(pattern: &str, home: &Path) -> Vec<PathBuf> {
    let path = match pattern.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if pattern.starts_with('/') => PathBuf::from(pattern),
        None => home.join(".ssh").join(pattern),
    };
    let wild = |s: &str| s.contains(['*', '?']);
    let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str())) else {
        return Vec::new();
    };
    if !wild(name) {
        return vec![path];
    }
    if dir.to_str().is_none_or(wild) {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        // As glob(3): a wildcard does not match a leading dot.
        .filter(|entry| (!entry.starts_with('.') || name.starts_with('.')) && glob(name, entry))
        .map(|entry| dir.join(entry))
        .collect();
    found.sort();
    found
}

/// `*` and `?` over one path component.
fn glob(pattern: &str, name: &str) -> bool {
    let (p, n): (Vec<char>, Vec<char>) = (pattern.chars().collect(), name.chars().collect());
    let (mut pi, mut ni, mut star, mut mark) = (0, 0, None, 0);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(at) = star {
            pi = at + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// What OpenSSH says an alias connects to, from `ssh -G`: its user, host
/// name and port. None when ssh cannot say.
pub async fn resolve(ssh: &Path, alias: &str) -> Option<Value> {
    let mut command = Command::new(ssh);
    command
        .args(["-G", "--", alias])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(5), command.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let field = |key: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix(' '))
            .map(str::to_owned)
    };
    Some(json!({"user": field("user"), "hostname": field("hostname"), "port": field("port")}))
}

/// Where a host's connection lives on this machine: its control socket, the
/// socket its daemon is forwarded to, and the lock that says which app
/// process owns them.
#[derive(Clone, Debug, PartialEq)]
pub struct Paths {
    pub ctl: PathBuf,
    pub socket: PathBuf,
    pub lock: PathBuf,
}

/// OpenSSH binds a control socket under a temporary name this much longer.
const CTL_SUFFIX: usize = 17;

fn paths(dir: &Path, alias: &str) -> Result<Paths, String> {
    // A plain short alias names its files; any other is hashed, so no alias
    // can reach outside the directory or collide with another.
    let plain = alias.len() <= 32
        && !alias.starts_with('.')
        && alias
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    let name = if plain {
        alias.to_owned()
    } else {
        let mut hash = 0xcbf29ce484222325u64;
        for byte in alias.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        format!("h-{hash:016x}")
    };
    let paths = Paths {
        ctl: dir.join(format!("{name}.ctl")),
        socket: dir.join(format!("{name}.sock")),
        lock: dir.join(format!("{name}.lock")),
    };
    // Unix socket addresses are short (104 bytes on macOS).
    let fits = |path: &Path, extra: usize| {
        let mut probe = path.as_os_str().to_owned();
        probe.push("x".repeat(extra));
        std::os::unix::net::SocketAddr::from_pathname(PathBuf::from(probe)).is_ok()
    };
    if !fits(&paths.ctl, CTL_SUFFIX) || !fits(&paths.socket, 0) {
        return Err(format!(
            "host_path_too_long: {} is too long a directory for a socket",
            dir.display()
        ));
    }
    Ok(paths)
}

/// Options every ssh the app runs takes: never a prompt, the app's control
/// socket whatever the user's config says about multiplexing, and no
/// `RemoteCommand` from it.
fn common(paths: &Paths) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "-o",
        "BatchMode=yes",
        "-o",
        &format!("ConnectTimeout={CONNECT_TIMEOUT}"),
        "-o",
        "RemoteCommand=none",
    ]
    .map(OsString::from)
    .into();
    args.push("-S".into());
    args.push(paths.ctl.clone().into());
    args
}

/// The one long-lived ssh per host: a ControlMaster in the foreground, with
/// keepalives, that the app supervises. Forwards from the user's config are
/// cleared, so one already bound elsewhere cannot stop it; the daemon's
/// forward is added once the host says where its socket is.
pub fn master_args(paths: &Paths, alias: &str) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "-M",
        "-N",
        "-o",
        "ControlPersist=no",
        "-o",
        &format!("ServerAliveInterval={ALIVE_INTERVAL}"),
        "-o",
        &format!("ServerAliveCountMax={ALIVE_COUNT}"),
        "-o",
        "ClearAllForwardings=yes",
        "-o",
        "ExitOnForwardFailure=yes",
        "-o",
        "StreamLocalBindUnlink=yes",
        "-o",
        "StreamLocalBindMask=0177",
    ]
    .map(OsString::from)
    .into();
    args.extend(common(paths));
    args.extend(["--".into(), alias.into()]);
    args
}

/// A command on the host over the master, never becoming a master itself.
pub fn exec_args(paths: &Paths, alias: &str, command: &str) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["-T", "-o", "ControlMaster=no"].map(OsString::from).into();
    args.extend(common(paths));
    args.extend(["--".into(), alias.into(), command.into()]);
    args
}

/// A request to the running master: `forward` or `cancel` the daemon's
/// socket to the local one, or `exit`.
pub fn control_args(paths: &Paths, alias: &str, op: &str, remote: Option<&str>) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "-S".into(),
        paths.ctl.clone().into(),
        "-O".into(),
        op.into(),
    ];
    if let Some(remote) = remote {
        let mut forward = paths.socket.clone().into_os_string();
        forward.push(":");
        forward.push(remote);
        args.extend(["-L".into(), forward]);
    }
    args.extend(["--".into(), alias.into()]);
    args
}

/// `agent ARGS` in the remote user's login shell, where a terminal there
/// would find it and its provider keys, as the app starts a local daemon
/// with the login shell's environment. The remote user's shell parses this,
/// so it is written to mean the same in sh, bash, zsh and fish.
pub fn remote_command(args: &str) -> String {
    format!(r#"exec "$SHELL" -l -i -c 'printf "\n{HOME_MARK}%s\n" "$HOME"; exec agent {args}'"#)
}

/// The host's daemon, as `agent start` there described it.
#[derive(Clone, Debug, PartialEq)]
pub struct Remote {
    pub socket: String,
    pub home: Option<String>,
}

fn missing(alias: &str) -> String {
    format!(
        "agent_missing: {alias} has no agent on its login shell's PATH. Install the Linux agent there (cargo install --locked --path . in a checkout of lydakis/agent); the window attaches once it answers"
    )
}

/// Why ssh itself failed, from the end of what it printed.
fn ssh_reason(alias: &str, stderr: &str) -> String {
    let last = stderr
        .lines()
        .map(|line| line.trim().trim_end_matches('.'))
        .rfind(|line| !line.is_empty())
        .unwrap_or("ssh exited");
    if stderr.contains("Permission denied (") {
        format!(
            "host_auth_failed: ssh {alias}: {last}. The app cannot answer a password prompt: load a key into ssh-agent (ssh-add) so ssh {alias} needs none"
        )
    } else if stderr.contains("Host key verification failed")
        || stderr.contains("HOST IDENTIFICATION HAS CHANGED")
    {
        format!(
            "host_key_unverified: ssh {alias}: {last}. Run ssh {alias} once in a terminal to check and accept its host key"
        )
    } else {
        format!("host_unreachable: ssh {alias}: {last}")
    }
}

/// The CLI's error line, `agent: CODE: DETAIL`.
fn cli_reason(stderr: &[u8]) -> Option<String> {
    String::from_utf8_lossy(stderr)
        .lines()
        .rev()
        .find_map(|line| line.trim().strip_prefix("agent: "))
        .map(str::to_owned)
}

/// Read `agent start`'s answer from the host. Its ready line says which
/// daemon holds the store there, even when the host's agent refused it for
/// another protocol, so an older daemon can be told from an older agent.
pub fn started(alias: &str, output: &Output) -> Result<Remote, String> {
    let stderr = String::from_utf8_lossy(&output.stderr);
    match output.status.code() {
        Some(255) => return Err(ssh_reason(alias, &stderr)),
        Some(127) => return Err(missing(alias)),
        _ => {}
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let home = stdout
        .lines()
        .find_map(|line| line.strip_prefix(HOME_MARK))
        .filter(|home| home.starts_with('/'))
        .map(str::to_owned);
    let ready = stdout.lines().rev().find_map(|line| {
        serde_json::from_str::<Value>(line)
            .ok()
            .filter(|value| value["event"] == "ready")
    });
    let reason = cli_reason(&output.stderr);
    let Some(ready) = ready else {
        return Err(match reason {
            Some(reason) if reason.starts_with("usage: no provider") => format!(
                "host_no_provider: {alias} has no provider for its daemon; export a provider key (or AGENT_PROVIDER) in its login shell's profile"
            ),
            Some(reason) => format!("{reason} (on {alias})"),
            None => format!(
                "daemon_start_failed: agent start on {alias} exited with {}",
                output.status
            ),
        });
    };
    let app = agent_client::PROTOCOL;
    match ready["protocol"].as_u64() {
        Some(protocol) if protocol > app => {
            return Err(format!(
                "daemon_newer: the daemon on {alias} speaks protocol {protocol}, this app {app}; update the app"
            ));
        }
        // The host's agent refused its own daemon: an upgrade there left the
        // older daemon running, which that agent can replace.
        Some(protocol) if protocol < app && !output.status.success() => {
            return Err(format!(
                "daemon_older: the daemon on {alias} speaks protocol {protocol}, this app {app}"
            ));
        }
        // The host's agent accepted it, so the agent itself is that old.
        Some(protocol) if protocol < app => {
            return Err(format!(
                "host_agent_older: the agent on {alias} speaks protocol {protocol}, this app {app}; install this version's Linux agent there"
            ));
        }
        Some(_) => {}
        None => return Err(format!("daemon_greeting_invalid: agent start on {alias}")),
    }
    let Some(socket) = ready["socket"].as_str().filter(|s| s.starts_with('/')) else {
        return Err(format!(
            "host_agent_older: the agent on {alias} does not say its daemon's socket; install this version's Linux agent there"
        ));
    };
    // `-L LOCAL:REMOTE` has no quoting for a colon.
    if socket.contains(':') {
        return Err(format!(
            "host_socket_unsupported: {alias}'s daemon listens on {socket}, which ssh cannot forward"
        ));
    }
    Ok(Remote {
        socket: socket.to_owned(),
        home,
    })
}

fn backoff(failures: u32) -> Duration {
    (BACKOFF_FIRST * 2u32.pow(failures.saturating_sub(1).min(5))).min(BACKOFF_LAST)
}

/// Every host a window of this app has opened, and how many windows each
/// has open. A host keeps its entry once opened, so its connection's lock
/// orders a close against a later open.
pub struct Hosts {
    dir: Option<PathBuf>,
    ssh: PathBuf,
    open: std::sync::Mutex<HashMap<String, Arc<Host>>>,
}

impl Hosts {
    /// `dir` holds each host's control socket, forwarded socket and lock,
    /// and is made private; `ssh` is the program run.
    pub fn new(dir: Option<PathBuf>, ssh: PathBuf) -> Self {
        Self {
            dir,
            ssh,
            open: Default::default(),
        }
    }

    pub fn ssh(&self) -> &Path {
        &self.ssh
    }

    /// A window opens `alias`: its host, with one more window.
    pub fn acquire(&self, alias: &str) -> Result<Arc<Host>, String> {
        if !valid_alias(alias) {
            return Err(format!("host_invalid: {alias:?} is not a host alias"));
        }
        let mut open = self.open.lock().unwrap();
        let host = match open.get(alias) {
            Some(host) => host.clone(),
            None => {
                let dir =
                    (self.dir.clone()).ok_or("host_files_unusable: no HOME for ~/.agent/hosts")?;
                let host = Arc::new(Host {
                    alias: alias.to_owned(),
                    ssh: self.ssh.clone(),
                    paths: paths(&dir, alias)?,
                    dir,
                    windows: AtomicUsize::new(0),
                    link: Default::default(),
                });
                open.insert(alias.to_owned(), host.clone());
                host
            }
        };
        host.windows.fetch_add(1, Ordering::SeqCst);
        Ok(host)
    }

    /// A window on `host` closed; the last one closes its connection.
    pub async fn release(&self, host: &Host) {
        let before = (host.windows)
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .unwrap_or(0);
        if before == 1 {
            host.close().await;
        }
    }

    /// The app is quitting: every connection closes. A host still
    /// connecting gets a few seconds; after that the app exits anyway, and
    /// a master it leaves behind is asked to exit when a window next opens
    /// that host.
    pub async fn close_all(&self) {
        let hosts: Vec<_> = self.open.lock().unwrap().values().cloned().collect();
        for host in hosts {
            host.windows.store(0, Ordering::SeqCst);
            let _ = tokio::time::timeout(Duration::from_secs(3), host.close()).await;
        }
    }
}

pub struct Host {
    pub alias: String,
    ssh: PathBuf,
    dir: PathBuf,
    paths: Paths,
    windows: AtomicUsize,
    link: tokio::sync::Mutex<Link>,
}

#[derive(Default)]
struct Link {
    /// Held while this process owns the host's files.
    lock: Option<std::fs::File>,
    master: Option<Master>,
    /// The remote socket the master forwards now.
    forwarded: Option<String>,
    remote: Option<Remote>,
    failures: u32,
    failed: Option<(Instant, String)>,
}

struct Master {
    child: Child,
    stderr: Arc<std::sync::Mutex<String>>,
    reader: tokio::task::JoinHandle<()>,
}

impl Link {
    /// Whether the master still runs; one that exited takes its forward.
    fn alive(&mut self) -> bool {
        let alive = (self.master.as_mut()).is_some_and(|m| matches!(m.child.try_wait(), Ok(None)));
        if !alive {
            self.master = None;
            self.forwarded = None;
        }
        alive
    }
}

impl Host {
    /// The forwarded socket while the master runs and forwards it, so an
    /// attach tries it before asking the host anything.
    pub async fn reached(&self) -> Option<PathBuf> {
        let mut link = self.link.lock().await;
        (link.alive() && link.forwarded.is_some()).then(|| self.paths.socket.clone())
    }

    /// The remote home, once `agent start` has said it.
    pub async fn home(&self) -> Option<String> {
        let link = self.link.lock().await;
        link.remote.as_ref().and_then(|r| r.home.clone())
    }

    /// Reach the host's daemon: the master connected (again, after it
    /// exited), `agent start` there, and its socket forwarded here. A
    /// failure stands, and is returned again, until its backoff has passed.
    pub async fn connect(&self) -> Result<PathBuf, String> {
        let mut link = self.link.lock().await;
        if let Some((until, reason)) = &link.failed
            && Instant::now() < *until
        {
            return Err(reason.clone());
        }
        let result = self.establish(&mut link).await;
        match &result {
            Ok(_) => {
                link.failures = 0;
                link.failed = None;
            }
            Err(reason) => {
                link.failures += 1;
                link.failed = Some((Instant::now() + backoff(link.failures), reason.clone()));
            }
        }
        result
    }

    async fn establish(&self, link: &mut Link) -> Result<PathBuf, String> {
        self.own(link)?;
        if !link.alive() {
            self.start_master(link).await?;
        }
        let output = self
            .run(&exec_args(
                &self.paths,
                &self.alias,
                &remote_command("start"),
            ))
            .await?;
        let remote = started(&self.alias, &output)?;
        if link.forwarded.as_deref() != Some(remote.socket.as_str()) {
            if let Some(old) = link.forwarded.take() {
                let _ = self.control("cancel", Some(&old)).await;
            }
            self.control("forward", Some(&remote.socket)).await?;
            link.forwarded = Some(remote.socket.clone());
        }
        link.remote = Some(remote);
        Ok(self.paths.socket.clone())
    }

    /// Stop the host's daemon with its own `agent shutdown`, so the next
    /// attach starts one with its own `agent start`. Nothing on this machine
    /// signals a process there.
    pub async fn replace(&self) -> Result<(), String> {
        let mut link = self.link.lock().await;
        link.failures = 0;
        link.failed = None;
        self.own(&mut link)?;
        if !link.alive() {
            self.start_master(&mut link).await?;
        }
        let output = self
            .run(&exec_args(
                &self.paths,
                &self.alias,
                &remote_command("shutdown"),
            ))
            .await?;
        match output.status.code() {
            Some(0) => Ok(()),
            Some(255) => Err(ssh_reason(
                &self.alias,
                &String::from_utf8_lossy(&output.stderr),
            )),
            Some(127) => Err(missing(&self.alias)),
            _ => Err(match cli_reason(&output.stderr) {
                Some(reason) if reason.starts_with("daemon_unavailable") => return Ok(()),
                Some(reason) => format!("{reason} (on {})", self.alias),
                None => format!(
                    "daemon_stop_failed: agent shutdown on {} exited with {}",
                    self.alias, output.status
                ),
            }),
        }
    }

    /// No window needs the host any more: its master ends, which ends the
    /// forward, and its files and lock go. The daemon there keeps running.
    pub async fn close(&self) {
        let mut link = self.link.lock().await;
        // A window opened again while this waited for the lock keeps it.
        if self.windows.load(Ordering::SeqCst) > 0 {
            return;
        }
        if let Some(mut master) = link.master.take() {
            stop(&mut master.child).await;
            master.reader.abort();
        }
        link.forwarded = None;
        link.remote = None;
        link.failures = 0;
        link.failed = None;
        if link.lock.is_some() {
            let _ = std::fs::remove_file(&self.paths.ctl);
            let _ = std::fs::remove_file(&self.paths.socket);
        }
        link.lock = None;
    }

    /// Only one app process may own a host's files: another's master would
    /// have its forwarded socket unlinked from under it.
    fn own(&self, link: &mut Link) -> Result<(), String> {
        use std::os::unix::{fs::OpenOptionsExt, io::AsRawFd};
        if link.lock.is_some() {
            return Ok(());
        }
        private_dir(&self.dir)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(&self.paths.lock)
            .map_err(|e| format!("host_files_unusable: {}: {e}", self.paths.lock.display()))?;
        // SAFETY: an advisory lock on a descriptor this function owns.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(format!(
                "host_busy: another Agent process has {} open; use its window or quit it",
                self.alias
            ));
        }
        link.lock = Some(file);
        Ok(())
    }

    async fn start_master(&self, link: &mut Link) -> Result<(), String> {
        use tokio::io::AsyncReadExt;
        // A control socket left by an app that did not close it: that master
        // is asked to exit, and whatever it left is removed.
        if std::fs::symlink_metadata(&self.paths.ctl).is_ok() {
            let _ = self.control("exit", None).await;
            let _ = std::fs::remove_file(&self.paths.ctl);
        }
        let _ = std::fs::remove_file(&self.paths.socket);
        link.forwarded = None;
        let mut child = Command::new(&self.ssh)
            .args(master_args(&self.paths, &self.alias))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("host_unreachable: {}: {e}", self.ssh.display()))?;
        let stderr = Arc::new(std::sync::Mutex::new(String::new()));
        let mut pipe = child.stderr.take().expect("piped");
        let kept = stderr.clone();
        let reader = tokio::spawn(async move {
            let mut buffer = [0u8; 1024];
            while let Ok(read) = pipe.read(&mut buffer).await {
                if read == 0 {
                    break;
                }
                let mut text = kept.lock().unwrap();
                text.push_str(&String::from_utf8_lossy(&buffer[..read]));
                if text.len() > STDERR_KEPT {
                    let cut = text.len() - STDERR_KEPT;
                    let cut = (cut..text.len())
                        .find(|at| text.is_char_boundary(*at))
                        .unwrap_or(text.len());
                    text.drain(..cut);
                }
            }
        });
        let mut master = Master {
            child,
            stderr,
            reader,
        };
        let deadline = Instant::now() + MASTER_TIMEOUT;
        loop {
            if std::fs::symlink_metadata(&self.paths.ctl).is_ok() {
                link.master = Some(master);
                return Ok(());
            }
            if let Ok(Some(_)) = master.child.try_wait() {
                // Everything it printed, before reading why.
                let _ = tokio::time::timeout(Duration::from_secs(1), &mut master.reader).await;
                let text = master.stderr.lock().unwrap().clone();
                return Err(ssh_reason(&self.alias, &text));
            }
            if Instant::now() > deadline {
                stop(&mut master.child).await;
                return Err(format!(
                    "host_unreachable: ssh {} did not connect within {} seconds",
                    self.alias,
                    MASTER_TIMEOUT.as_secs()
                ));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn control(&self, op: &str, remote: Option<&str>) -> Result<(), String> {
        let output = self
            .run(&control_args(&self.paths, &self.alias, op, remote))
            .await?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let last = stderr.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("");
        Err(format!(
            "host_forward_failed: ssh {} -O {op}: {last}",
            self.alias
        ))
    }

    async fn run(&self, args: &[OsString]) -> Result<Output, String> {
        let mut command = Command::new(&self.ssh);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        tokio::time::timeout(COMMAND_TIMEOUT, command.output())
            .await
            .map_err(|_| format!("host_timeout: ssh {} did not answer", self.alias))?
            .map_err(|e| format!("host_unreachable: {}: {e}", self.ssh.display()))
    }
}

/// SIGTERM, so ssh removes its control socket, then SIGKILL if it lingers.
async fn stop(child: &mut Child) {
    if let Some(pid) = child.id().and_then(|pid| libc::pid_t::try_from(pid).ok()) {
        // SAFETY: a signal to the ssh this process started and has not reaped.
        unsafe { libc::kill(pid, libc::SIGTERM) };
        if tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .is_ok()
        {
            return;
        }
    }
    let _ = child.kill().await;
}

/// The directory for hosts' sockets: made owner-only, and refused if it is
/// anyone else's or others can enter it.
fn private_dir(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => {
            return Err(format!("{}: {error}", dir.display()));
        }
        _ => {}
    }
    let metadata = std::fs::symlink_metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    // SAFETY: geteuid only reads this process's effective user.
    let uid = unsafe { libc::geteuid() };
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err(format!(
            "unsafe_host_directory: {} must be a directory only you can use",
            dir.display()
        ));
    }
    Ok(())
}

/// A stand-in `ssh` for tests: it runs the remote command here, in a home of
/// its own with a stand-in `agent` on its PATH, and forwards `-L A:B` by
/// linking A to B, since a link to a Unix socket connects to that socket. As
/// a master it stays up, like `ssh -N`, until signalled or its control
/// socket is removed; `mode` set to `auth` makes it refuse the login.
#[cfg(test)]
pub mod shim {
    use std::path::{Path, PathBuf};

    pub struct Shim {
        pub root: PathBuf,
        pub ssh: PathBuf,
        pub remote: PathBuf,
    }

    /// An executable written by a child process, so no other test's child
    /// inherits it open for writing ("Text file busy").
    pub fn script(path: &Path, text: &str) {
        use std::io::Write;
        let _ = std::fs::remove_file(path);
        let mut writer = std::process::Command::new("/bin/sh")
            .args(["-c", "cat > \"$0\" && chmod 755 \"$0\""])
            .arg(path)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        (writer.stdin.take().unwrap())
            .write_all(text.as_bytes())
            .unwrap();
        assert!(writer.wait().unwrap().success());
    }

    impl Shim {
        /// Under /tmp, whose short path leaves room for socket names.
        pub fn new(name: &str) -> Self {
            let root = PathBuf::from(format!("/tmp/agent-app-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let remote = root.join("remote");
            std::fs::create_dir_all(remote.join("bin")).unwrap();
            std::fs::create_dir_all(root.join("bin")).unwrap();
            let ssh = root.join("bin/ssh");
            let r = root.display();
            script(
                &ssh,
                &format!(
                    r#"#!/bin/sh
root='{r}'
master= ctl= op= fwd= resolve= host=
while [ $# -gt 0 ]; do
  case "$1" in
    -M) master=1 ;;
    -N|-T|-n) ;;
    -G) resolve=1 ;;
    -S) ctl=$2; shift ;;
    -o) shift ;;
    -O) op=$2; shift ;;
    -L) fwd=$2; shift ;;
    --) host=$2; shift 2; break ;;
    *) host=$1; shift; break ;;
  esac
  shift
done
mode=$(cat "$root/mode" 2>/dev/null)
if [ -n "$resolve" ]; then printf 'user someone\nhostname %s.example\nport 22\n' "$host"; exit 0; fi
refuse() {{ echo "someone@$host: Permission denied (publickey)." >&2; exit 255; }}
cleanup() {{ [ -f "$ctl.fwd" ] && rm -f $(cat "$ctl.fwd") "$ctl.fwd"; rm -f "$ctl"; }}
if [ -n "$master" ]; then
  echo master >> "$root/log"
  [ "$mode" = auth ] && refuse
  trap 'cleanup; exit 0' TERM INT HUP
  echo $$ > "$ctl"
  while [ -e "$ctl" ]; do sleep 0.05; done
  cleanup; exit 255
fi
if [ -n "$op" ]; then
  echo "$op" >> "$root/log"
  [ -e "$ctl" ] || {{ echo "Control socket connect($ctl): No such file or directory" >&2; exit 255; }}
  case "$op" in
    forward) ln -sfn "${{fwd#*:}}" "${{fwd%%:*}}" && echo "${{fwd%%:*}}" >> "$ctl.fwd" ;;
    cancel) rm -f "${{fwd%%:*}}" ;;
    exit) kill "$(cat "$ctl")" 2>/dev/null; rm -f "$ctl" ;;
  esac
  exit 0
fi
[ "$mode" = auth ] && refuse
[ -e "$ctl" ] || {{ echo "no master for $host" >&2; exit 255; }}
echo exec >> "$root/log"
cd "$root/remote" && HOME="$root/remote" SHELL=/bin/sh PATH="$root/remote/bin:/usr/bin:/bin" exec /bin/sh -c "$*"
"#
                ),
            );
            Self { root, ssh, remote }
        }

        /// The host's `agent`: `start` prints `ready` and exits `status`.
        pub fn agent(&self, ready: &str, status: u8) {
            std::fs::write(self.root.join("ready"), format!("{ready}\n")).unwrap();
            let r = self.root.display();
            script(
                &self.remote.join("bin/agent"),
                &format!(
                    "#!/bin/sh\necho \"agent $*\" >> '{r}/log'\n[ \"$1\" = start ] || exit 0\ncat '{r}/ready'\n[ {status} = 0 ] || echo 'agent: daemon_protocol_mismatch: the daemon speaks protocol 1' >&2\nexit {status}\n"
                ),
            );
        }

        pub fn mode(&self, mode: &str) {
            std::fs::write(self.root.join("mode"), mode).unwrap();
        }

        /// What ssh and the host's agent were asked to do, one per line.
        pub fn log(&self) -> Vec<String> {
            std::fs::read_to_string(self.root.join("log"))
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }

        pub fn count(&self, what: &str) -> usize {
            self.log().iter().filter(|line| *line == what).count()
        }

        pub fn hosts(&self) -> super::Hosts {
            super::Hosts::new(Some(self.root.join("hosts")), self.ssh.clone())
        }
    }

    impl Drop for Shim {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// A daemon's greeting on `socket`, for every connection, held open.
    pub fn daemon(socket: &Path, store: &str) {
        use std::io::Write;
        let listener = std::os::unix::net::UnixListener::bind(socket).unwrap();
        let ready = serde_json::json!({"event": "ready", "protocol": agent_client::PROTOCOL,
            "pid": std::process::id(), "store": {"identity": store}});
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let _ = writeln!(stream, "{ready}");
                held.push(stream);
            }
        });
    }

    /// `agent start`'s answer for the stand-in daemon at `socket`.
    pub fn ready(socket: &Path, protocol: u64) -> String {
        serde_json::json!({"event": "ready", "protocol": protocol, "pid": 1,
            "store": {"identity": "s1"}, "socket": socket})
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::shim::{Shim, daemon, ready};
    use super::*;

    #[test]
    fn the_host_list_is_the_configs_concrete_aliases_and_what_it_includes() {
        let home = PathBuf::from(format!("/tmp/agent-app-aliases-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join(".ssh/config.d")).unwrap();
        std::fs::create_dir_all(home.join("other")).unwrap();
        std::fs::write(
            home.join(".ssh/config"),
            "# hosts\nHost box build-1 *.internal !gone\n  HostName 10.0.0.5\n\
             Host=eq\nhost \"quoted\" # trailing\nHost ?x -oProxyCommand=x a/b\n\
             Include config.d/*.conf ~/other/extra\nInclude missing\nMatch host box\nHost box\n\
             Include config\n",
        )
        .unwrap();
        std::fs::write(home.join(".ssh/config.d/b.conf"), "Host second\n").unwrap();
        std::fs::write(home.join(".ssh/config.d/a.conf"), "Host first\n").unwrap();
        std::fs::write(home.join(".ssh/config.d/.hidden.conf"), "Host hidden\n").unwrap();
        std::fs::write(home.join(".ssh/config.d/c.txt"), "Host notconf\n").unwrap();
        std::fs::write(home.join("other/extra"), "Host extra\n").unwrap();
        assert_eq!(
            aliases(&home.join(".ssh/config"), &home),
            ["box", "build-1", "eq", "quoted", "first", "second", "extra"]
        );
        assert!(aliases(&home.join("none"), &home).is_empty());
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn an_alias_is_one_concrete_destination_never_an_option() {
        for good in ["box", "build-1.example", "user@box", "[::1]"] {
            assert!(valid_alias(good), "{good}");
        }
        for bad in [
            "",
            "-oProxyCommand=x",
            "a*",
            "b?",
            "!c",
            "a b",
            "a/b",
            "a\nb",
        ] {
            assert!(!valid_alias(bad), "{bad:?}");
        }
        assert!(glob("*.conf", "a.conf") && glob("a?c*", "abcdef") && !glob("*.conf", "a.txt"));
    }

    #[test]
    fn ssh_is_asked_for_one_master_that_never_prompts_and_forwards_the_socket() {
        let dir = Path::new("/tmp/agent-hosts");
        let p = paths(dir, "box").unwrap();
        assert_eq!(p.ctl, dir.join("box.ctl"));
        assert_eq!(p.socket, dir.join("box.sock"));
        // An alias that is not a plain name cannot reach outside the directory.
        let odd = paths(dir, "user@box.example:22").unwrap();
        assert_eq!(odd.ctl.parent(), Some(dir));
        assert!(
            odd.ctl
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("h-")
        );
        assert!(paths(&PathBuf::from(format!("/tmp/{}", "d".repeat(100))), "box").is_err());
        let text = |args: Vec<OsString>| {
            args.iter()
                .map(|a| a.to_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        let master = text(master_args(&p, "box"));
        for option in [
            "BatchMode=yes",
            "ControlPersist=no",
            "ServerAliveInterval=15",
            "ServerAliveCountMax=3",
            "ExitOnForwardFailure=yes",
            "StreamLocalBindUnlink=yes",
            "ClearAllForwardings=yes",
            "RemoteCommand=none",
        ] {
            let at = master.iter().position(|a| a == option).expect(option);
            assert_eq!(master[at - 1], "-o");
        }
        assert_eq!(&master[..2], ["-M", "-N"]);
        assert_eq!(&master[master.len() - 2..], ["--", "box"]);
        let at = master.iter().position(|a| a == "-S").unwrap();
        assert_eq!(master[at + 1], "/tmp/agent-hosts/box.ctl");
        let exec = text(exec_args(&p, "box", "agent start"));
        assert!(
            exec.contains(&"ControlMaster=no".to_owned())
                && exec.contains(&"BatchMode=yes".to_owned())
        );
        assert_eq!(&exec[exec.len() - 3..], ["--", "box", "agent start"]);
        assert_eq!(
            text(control_args(
                &p,
                "box",
                "forward",
                Some("/home/u/.agent/state.sqlite.sock")
            )),
            [
                "-S",
                "/tmp/agent-hosts/box.ctl",
                "-O",
                "forward",
                "-L",
                "/tmp/agent-hosts/box.sock:/home/u/.agent/state.sqlite.sock",
                "--",
                "box"
            ]
        );
        let command = remote_command("start");
        assert!(
            command.starts_with(r#"exec "$SHELL" -l -i -c '"#),
            "{command}"
        );
        assert!(command.ends_with("; exec agent start'"), "{command}");
        assert_eq!(backoff(1), Duration::from_secs(1));
        assert_eq!(backoff(3), Duration::from_secs(4));
        assert_eq!(backoff(40), BACKOFF_LAST);
    }

    fn output(status: i32, stdout: &str, stderr: &str) -> Output {
        use std::os::unix::process::ExitStatusExt;
        Output {
            status: std::process::ExitStatus::from_raw(status << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn agent_start_on_the_host_says_where_its_daemon_is_or_why_not() {
        let now = agent_client::PROTOCOL;
        let line = |protocol: u64| {
            format!(
                r#"{{"event":"ready","protocol":{protocol},"store":{{"identity":"s1"}},"socket":"/run/a.sock"}}"#
            )
        };
        let ok = format!("motd\n\n{HOME_MARK}/home/u\n{}\n", line(now));
        assert_eq!(
            started("box", &output(0, &ok, "bash: no job control")),
            Ok(Remote {
                socket: "/run/a.sock".into(),
                home: Some("/home/u".into()),
            })
        );
        let code = |result: Result<Remote, String>| {
            result.unwrap_err().split(':').next().unwrap().to_owned()
        };
        assert_eq!(
            code(started("box", &output(127, "", "sh: agent: not found"))),
            "agent_missing"
        );
        assert!(
            started("box", &output(127, "", ""))
                .unwrap_err()
                .contains("install the Linux agent there")
                || started("box", &output(127, "", ""))
                    .unwrap_err()
                    .contains("Install the Linux agent there")
        );
        assert_eq!(
            code(started(
                "box",
                &output(255, "", "u@box: Permission denied (publickey).")
            )),
            "host_auth_failed"
        );
        assert_eq!(
            code(started(
                "box",
                &output(2, "", "agent: usage: no provider: pass")
            )),
            "host_no_provider"
        );
        // The host's agent accepted an older daemon: that agent is old.
        assert_eq!(
            code(started("box", &output(0, &line(now - 1), ""))),
            "host_agent_older"
        );
        // It refused one: an upgrade there left the old daemon, which it can replace.
        let refused = "agent: daemon_protocol_mismatch: the daemon speaks protocol 3";
        assert_eq!(
            code(started("box", &output(1, &line(now - 1), refused))),
            "daemon_older"
        );
        assert_eq!(
            code(started("box", &output(0, &line(now + 1), ""))),
            "daemon_newer"
        );
        // One the app can talk to is used, whatever the host's agent thought of it.
        assert!(started("box", &output(1, &line(now), refused)).is_ok());
        let quiet = format!(r#"{{"event":"ready","protocol":{now}}}"#);
        assert_eq!(
            code(started("box", &output(0, &quiet, ""))),
            "host_agent_older"
        );
        assert_eq!(
            started(
                "box",
                &output(1, "", "agent: daemon_start_failed: disk full")
            )
            .unwrap_err(),
            "daemon_start_failed: disk full (on box)"
        );
    }

    #[tokio::test]
    async fn a_host_is_attached_through_its_master_and_reattached_after_the_master_exits() {
        let shim = Shim::new("host-attach");
        let socket = shim.remote.join("daemon.sock");
        daemon(&socket, "s1");
        shim.agent(&ready(&socket, agent_client::PROTOCOL), 0);
        let hosts = shim.hosts();
        let host = hosts.acquire("box").unwrap();
        assert_eq!(host.reached().await, None);
        let local = host.connect().await.unwrap();
        assert_eq!(local, shim.root.join("hosts/box.sock"));
        let (client, _events) = agent_client::Client::connect(&local).await.unwrap();
        assert_eq!(client.store(), Some("s1"));
        assert_eq!(host.home().await.as_deref(), shim.remote.to_str());
        assert_eq!(host.reached().await, Some(local.clone()));
        assert_eq!(shim.log(), ["master", "exec", "agent start", "forward"]);
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(shim.root.join("hosts"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        // The master dies without cleaning up, as after a crash: the next
        // connect finds it gone, starts another and forwards again.
        let pid: i32 = std::fs::read_to_string(shim.root.join("hosts/box.ctl"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // SAFETY: the stand-in master this test started.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        let deadline = Instant::now() + Duration::from_secs(5);
        while host.reached().await.is_some() {
            assert!(Instant::now() < deadline, "the master's exit is noticed");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let again = host.connect().await.unwrap();
        agent_client::Client::connect(&again).await.unwrap();
        assert_eq!(shim.count("master"), 2);
        assert_eq!(shim.count("forward"), 2);
        // The last window closes the connection and its files; the daemon stays.
        hosts.release(&host).await;
        assert!(!shim.root.join("hosts/box.ctl").exists());
        assert!(std::fs::symlink_metadata(shim.root.join("hosts/box.sock")).is_err());
        assert_eq!(host.reached().await, None);
        assert_eq!(shim.count("agent shutdown"), 0);
    }

    #[tokio::test]
    async fn a_refused_login_or_a_missing_agent_is_reported_and_not_retried_at_once() {
        let shim = Shim::new("host-refused");
        shim.mode("auth");
        let hosts = shim.hosts();
        let host = hosts.acquire("box").unwrap();
        let refused = host.connect().await.unwrap_err();
        assert!(
            refused.starts_with("host_auth_failed: ssh box: someone@box: Permission denied"),
            "{refused}"
        );
        assert!(refused.contains("ssh-add"), "{refused}");
        // Within the backoff the reason stands and nothing is run.
        assert_eq!(host.connect().await.unwrap_err(), refused);
        assert_eq!(shim.count("master"), 1);
        drop(hosts);

        let shim = Shim::new("host-missing");
        let hosts = shim.hosts();
        let host = hosts.acquire("box").unwrap();
        let missing = host.connect().await.unwrap_err();
        assert!(
            missing.starts_with("agent_missing: box has no agent"),
            "{missing}"
        );
        assert!(
            missing.contains("Install the Linux agent there"),
            "{missing}"
        );
        assert_eq!(shim.count("forward"), 0);
        hosts.close_all().await;
    }

    #[tokio::test]
    async fn an_older_daemon_on_a_host_is_replaced_by_the_hosts_own_agent() {
        let shim = Shim::new("host-older");
        let socket = shim.remote.join("daemon.sock");
        shim.agent(&ready(&socket, agent_client::PROTOCOL - 1), 1);
        let hosts = shim.hosts();
        let host = hosts.acquire("box").unwrap();
        let older = host.connect().await.unwrap_err();
        assert!(
            older.starts_with("daemon_older: the daemon on box"),
            "{older}"
        );
        // Restart: the host's `agent shutdown`, then its `agent start` on attach.
        host.replace().await.unwrap();
        assert_eq!(shim.count("agent shutdown"), 1);
        daemon(&socket, "s2");
        shim.agent(&ready(&socket, agent_client::PROTOCOL), 0);
        let local = host.connect().await.unwrap();
        let (client, _events) = agent_client::Client::connect(&local).await.unwrap();
        assert_eq!(client.store(), Some("s2"));
        assert_eq!(shim.count("master"), 1);
        hosts.close_all().await;
    }

    #[tokio::test]
    async fn one_app_process_owns_a_hosts_connection() {
        let shim = Shim::new("host-owned");
        let socket = shim.remote.join("daemon.sock");
        daemon(&socket, "s1");
        shim.agent(&ready(&socket, agent_client::PROTOCOL), 0);
        let (first, second) = (shim.hosts(), shim.hosts());
        let host = first.acquire("box").unwrap();
        host.connect().await.unwrap();
        let other = second.acquire("box").unwrap();
        let busy = other.connect().await.unwrap_err();
        assert!(busy.starts_with("host_busy: "), "{busy}");
        assert_eq!(shim.count("master"), 1);
        first.close_all().await;
    }
}
