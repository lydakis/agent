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
    collections::{HashMap, HashSet},
    ffi::OsString,
    path::{Path, PathBuf},
    process::{Output, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::process::{Child, Command};

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
/// How long an ssh the app ends is given to close its connection and remove
/// its control socket before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(2);
/// How long a master an earlier run left is given to exit when asked.
const RETIRE_TIMEOUT: Duration = Duration::from_secs(5);
/// Quitting: every host closes at once, within this.
const QUIT_TIMEOUT: Duration = Duration::from_secs(3);
/// Bounds on reading `~/.ssh/config` and what it includes.
const MAX_CONFIG: u64 = 1024 * 1024;
/// How much config is read in all, across includes.
const MAX_CONFIG_TOTAL: u64 = 4 * MAX_CONFIG;
/// How many hosts the list names.
const MAX_ALIASES: usize = 1024;
const MAX_INCLUDE_DEPTH: usize = 16;
const MAX_CONFIG_ENTRIES: usize = 4096;
/// Printed before `agent start`, so the app learns the remote home, the
/// folder a window on the host starts in, whatever a profile prints.
const HOME_MARK: &str = "__agent_app_home__";

/// Whether `alias` is a name to hand `ssh` as its destination: one concrete
/// host, never an option or a pattern. Not `user@host`, which ssh reads as a
/// user and a host rather than matching a `Host` entry of that name.
pub fn valid_alias(alias: &str) -> bool {
    !alias.is_empty()
        && alias.len() <= 255
        && !alias.starts_with('-')
        && !alias.starts_with('!')
        && !alias
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '*' | '?' | '/' | '@'))
}

/// The concrete `Host` aliases of an OpenSSH client config, in file order,
/// without patterns (`*`, `?`) or negations (`!`), which are not hosts one
/// can open. `Include` is followed as OpenSSH does for a user config: a
/// relative path from `~/.ssh`, `~/` from home, and wildcards in the last
/// component only; wildcards in a directory are not expanded. An `Include`
/// applies only where it stands, so one inside a `Host` or `Match` block is
/// followed only when that block applies to every host (`Host *`, `Match
/// all`); any other is conditional and skipped.
/// At most `MAX_ALIASES` are listed, from at most `MAX_CONFIG_TOTAL` bytes
/// of config across every included file.
pub fn aliases(config: &Path, home: &Path) -> Vec<String> {
    let mut found = Found {
        list: Vec::new(),
        seen: HashSet::new(),
        budget: MAX_CONFIG_TOTAL,
        entries: MAX_CONFIG_ENTRIES,
    };
    read_config(config, home, 0, &mut found);
    found.list
}

/// The aliases found so far, in order, and the config bytes left to read.
struct Found {
    list: Vec<String>,
    seen: HashSet<String>,
    budget: u64,
    entries: usize,
}

fn read_config(path: &Path, home: &Path, depth: usize, found: &mut Found) {
    use std::io::Read;
    if depth > MAX_INCLUDE_DEPTH || found.budget == 0 || found.list.len() >= MAX_ALIASES {
        return;
    }
    // Checked before opening: opening a FIFO would block.
    if !std::fs::metadata(path).is_ok_and(|m| m.is_file()) {
        return;
    }
    let mut text = String::new();
    let Ok(file) = std::fs::File::open(path) else {
        return;
    };
    let allowed = found.budget.min(MAX_CONFIG);
    let read = file.take(allowed + 1).read_to_string(&mut text);
    // An invalid UTF-8/read failure still spent the shared I/O budget.
    found.budget = found.budget.saturating_sub(if read.is_err() {
        allowed
    } else {
        text.len() as u64
    });
    if read.is_err() {
        return;
    }
    if text.len() as u64 > allowed {
        // Never publish a destination assembled from a cut-off Host line.
        let end = text.as_bytes()[..allowed as usize]
            .iter()
            .rposition(|b| *b == b'\n');
        text.truncate(end.map_or(0, |i| i + 1));
    }
    // Whether the block this line is in applies to every host.
    let mut everywhere = true;
    for line in text.lines() {
        let words = words(line);
        let Some((keyword, args)) = words.split_first() else {
            continue;
        };
        match keyword.to_ascii_lowercase().as_str() {
            "host" => {
                everywhere = args.len() == 1 && args[0] == "*";
                for alias in args {
                    if found.list.len() >= MAX_ALIASES {
                        return;
                    }
                    if valid_alias(alias) && found.seen.insert(alias.clone()) {
                        found.list.push(alias.clone());
                    }
                }
            }
            "match" => {
                everywhere = args.len() == 1 && args[0].eq_ignore_ascii_case("all");
            }
            "include" if everywhere => {
                for pattern in args {
                    for included in expand(pattern, home, &mut found.entries) {
                        read_config(&included, home, depth + 1, found);
                    }
                }
            }
            _ => {}
        }
    }
}

/// A config line's words: the keyword may end at `=`, and the rest is split
/// as OpenSSH's `argv_split` does. Words part at spaces and tabs; `"` and `'`
/// quote spaces, even mid-word; `\` escapes a quote, a backslash, or (outside
/// quotes) a space, and is kept before anything else; a `#` starting a word
/// starts a comment. A line with an open quote, which OpenSSH rejects, has
/// no words.
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
        while chars.next_if(|c| matches!(c, ' ' | '\t')).is_some() {}
        if matches!(chars.peek(), None | Some('#')) {
            break;
        }
        let (mut word, mut quote) = (String::new(), None);
        while let Some(c) = chars.next() {
            match c {
                '\\' => match chars.peek() {
                    Some(&next @ ('\'' | '"' | '\\')) => {
                        word.push(next);
                        chars.next();
                    }
                    Some(' ') if quote.is_none() => {
                        word.push(' ');
                        chars.next();
                    }
                    _ => word.push('\\'),
                },
                ' ' | '\t' if quote.is_none() => break,
                '"' | '\'' if quote.is_none() => quote = Some(c),
                _ if quote == Some(c) => quote = None,
                _ => word.push(c),
            }
        }
        if quote.is_some() {
            return Vec::new();
        }
        out.push(word);
    }
    out
}

fn expand(pattern: &str, home: &Path, budget: &mut usize) -> Vec<PathBuf> {
    if *budget == 0 {
        return Vec::new();
    }
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
        *budget -= 1;
        return vec![path];
    }
    if dir.to_str().is_none_or(wild) {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .take(*budget)
        .inspect(|_| *budget -= 1)
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
async fn resolve(ssh: &Path, alias: &str, registry: &Registry) -> Option<Value> {
    let args = ["-G", "--", alias].map(OsString::from);
    // Its whole config is a few KiB.
    let output = bounded(ssh, &args, MAX_RESOLVED, Duration::from_secs(5), registry)
        .await
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

impl Paths {
    fn named(dir: &Path, name: &str) -> Self {
        Self {
            ctl: dir.join(format!("{name}.ctl")),
            socket: dir.join(format!("{name}.sock")),
            lock: dir.join(format!("{name}.lock")),
        }
    }
}

/// OpenSSH binds a control socket under a temporary name this much longer.
const CTL_SUFFIX: usize = 17;

fn paths(dir: &Path, alias: &str) -> Result<Paths, String> {
    // Named by a hash of the exact alias, so no alias can reach outside the
    // directory, and two that differ only in case (`Box`, `box`) do not
    // share files on a case-insensitive file system. Short, for the socket
    // address limit.
    let mut hash = 0xcbf29ce484222325u64;
    for byte in alias.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let paths = Paths::named(dir, &format!("h-{hash:016x}"));
    // ssh expands `%` and `${` in a control path, and `%` in a forward's.
    {
        use std::os::unix::ffi::OsStrExt;
        if dir
            .as_os_str()
            .as_bytes()
            .iter()
            .any(|b| matches!(b, b'%' | b'$' | b':'))
        {
            return Err(format!(
                "host_path_unusable: {} contains %, $, or :, which ssh interprets in forwarding paths",
                dir.display()
            ));
        }
    }
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

/// Every option the app forces on the ssh processes that connect, all here:
/// `(option, master, exec, probed)`. A `-o` on the command line is read
/// before any config file and OpenSSH keeps an option's first value, so
/// `~/.ssh/config` cannot change these; the one exception is noted below.
/// `probed` marks an option only newer OpenSSH knows: an ssh that does not
/// know it cannot be told otherwise by its config either, so there it is
/// left out (see `Features`).
///
/// Not forced, as the user's own: how the host is reached (`ProxyCommand`,
/// `ProxyJump`, `Match exec`, `KnownHostsCommand`), keys and agents, and
/// environment (`SetEnv`, `SendEnv`). What those start here runs in the
/// ssh's process group and ends with it. `StdinNull` and `EscapeChar` need
/// nothing: stdin is `/dev/null` and there is no terminal. The control
/// socket is always `-S`, which the command line also decides.
const FORCED: &[(&str, bool, bool, bool)] = &[
    // No terminal here to answer a prompt.
    ("BatchMode=yes", true, true, false),
    ("ConnectTimeout=10", true, true, false),
    // Reasons are read from ssh's error lines: `QUIET` would hide them and
    // a debug level would bury them.
    ("LogLevel=ERROR", true, true, false),
    ("RequestTTY=no", true, true, false),
    // The exec's command is the app's; the master runs none (`-N`).
    ("RemoteCommand=none", true, true, false),
    // The ssh the app started is the one that does the work: it never
    // backgrounds itself, and no master outlives it.
    ("ForkAfterAuthentication=no", true, true, true),
    ("ControlPersist=no", true, true, false),
    // One master per host, the app's. A command uses it, or, if it has just
    // gone, connects on its own for that command alone.
    ("ControlMaster=yes", true, false, false),
    ("ControlMaster=no", false, true, false),
    // A command runs even on a host kept for forwarding (`SessionType none`).
    ("SessionType=default", false, true, true),
    // Forwards, tunnels, agents and X11 from a config are not the app's: a
    // forward bound elsewhere would end the master, and the daemon an exec
    // starts must not inherit a forwarded agent or display.
    ("ClearAllForwardings=yes", true, true, false),
    ("Tunnel=no", true, true, false),
    ("ForwardAgent=no", true, true, false),
    ("ForwardX11=no", true, true, false),
    // Nothing runs here on a config's say beyond reaching the host: no
    // `LocalCommand`, no `ssh-askpass` for `AddKeysToAgent ask`.
    ("PermitLocalCommand=no", true, true, false),
    ("AddKeysToAgent=no", true, true, false),
    // A link that died without a word is noticed within a minute, and an
    // idle forwarded connection is never closed for idling.
    ("ServerAliveInterval=15", true, false, false),
    ("ServerAliveCountMax=3", true, false, false),
    ("ChannelTimeout=global=0 *=0", true, false, true),
    // The daemon's forward: one that cannot bind ends the master, a socket
    // an earlier master left is replaced, and only this user may connect.
    // OpenSSH takes the last `StreamLocalBindMask` it reads, so a config can
    // loosen it; the owner-only directory the socket is in keeps others out.
    ("ExitOnForwardFailure=yes", true, false, false),
    ("StreamLocalBindUnlink=yes", true, false, false),
    ("StreamLocalBindMask=0177", true, false, false),
];

/// Which of the probed options (see `FORCED`) this ssh does not know.
#[derive(Debug, Default)]
pub struct Features {
    unknown: Vec<&'static str>,
}

impl Features {
    fn knows(&self, option: &str) -> bool {
        !self.unknown.contains(&option)
    }
}

/// Ask this ssh which probed options it knows. `-F none -G` reads no config
/// and connects to nothing.
async fn probe(ssh: &Path, registry: &Registry) -> Result<Features, String> {
    let mut unknown = Vec::new();
    for (option, .., probed) in FORCED {
        if !probed {
            continue;
        }
        let args = ["-F", "none", "-G", "-o", option, "--", "probe"].map(OsString::from);
        let output = bounded(ssh, &args, MAX_RESOLVED, Duration::from_secs(5), registry)
            .await
            .map_err(|_| format!("host_probe_failed: could not check SSH option {option}"))?;
        if output.status.success() {
            continue;
        }
        let error = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
        let key = option.split('=').next().unwrap().to_ascii_lowercase();
        if error.contains(&format!("bad configuration option: {key}")) {
            unknown.push(*option);
        } else {
            return Err(format!(
                "host_probe_failed: could not check SSH option {option}"
            ));
        }
    }
    Ok(Features { unknown })
}

fn forced(master: bool, features: &Features) -> Vec<OsString> {
    let mut args = Vec::new();
    for (option, for_master, for_exec, _) in FORCED {
        if (if master { *for_master } else { *for_exec }) && features.knows(option) {
            args.extend(["-o".into(), (*option).into()]);
        }
    }
    args
}

/// The one long-lived ssh per host: a ControlMaster in the foreground, with
/// keepalives and no session, that the app supervises. The daemon's forward
/// is added once the host says where its socket is.
pub fn master_args(paths: &Paths, alias: &str, features: &Features) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["-N".into()];
    args.extend(forced(true, features));
    args.extend([
        "-S".into(),
        paths.ctl.clone().into(),
        "--".into(),
        alias.into(),
    ]);
    args
}

/// A command on the host over the master, never becoming a master itself.
pub fn exec_args(paths: &Paths, alias: &str, command: &str, features: &Features) -> Vec<OsString> {
    let mut args = forced(false, features);
    args.extend([
        "-S".into(),
        paths.ctl.clone().into(),
        "--".into(),
        alias.into(),
        command.into(),
    ]);
    args
}

/// A request to the running master: `forward` or `cancel` the daemon's
/// socket to the local one, or `exit`. It reads no config at all
/// (`-F none`): the control socket is all it needs, so no `Match exec` runs
/// and no option changes it (a config's `ClearAllForwardings` would drop
/// the `-L` it carries).
pub fn control_args(paths: &Paths, alias: &str, op: &str, remote: Option<&str>) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "-F".into(),
        "none".into(),
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
    let reason = crate::daemon::cli_reason(&output.stderr);
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
    // `-L LOCAL:REMOTE` has no quoting for a colon, and ssh expands `%` in it.
    if socket.contains([':', '%']) {
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
///
/// Every process the app starts for a host is an ssh leading a process group
/// of its own, owned by exactly one thing that ends it on every path:
///
/// - `ssh -G` (the Hosts list): the list's request, bounded in time and bytes.
/// - The master: its host's `Link`, until the last window closes, the app
///   quits, or it exits (then the next attach starts another).
/// - `agent start` and `agent shutdown` on the host, and `-O forward`,
///   `cancel` and `exit` to the master: the attach or restart that ran
///   them, bounded in time and bytes.
///
/// What OpenSSH starts under one (`Match exec`, `ProxyCommand`,
/// `KnownHostsCommand`) is in its group and ends with it. Quitting ends
/// every group still running. A crash leaves them; the next launch retires
/// a master left behind and removes its files (`sweep`).
pub struct Hosts {
    env: Arc<Env>,
    open: std::sync::Mutex<HashMap<String, Arc<Host>>>,
}

/// What every host shares: where their files are, the ssh run, the process
/// groups started and not yet reaped, and what is done once.
struct Env {
    dir: Option<PathBuf>,
    ssh: PathBuf,
    registry: Registry,
    swept: tokio::sync::OnceCell<()>,
    features: tokio::sync::OnceCell<Features>,
}

impl Env {
    /// Once per launch: what an earlier run left is retired and removed.
    async fn swept(&self) {
        self.swept
            .get_or_init(|| async {
                if let Some(dir) = &self.dir {
                    sweep(dir, &self.ssh, &self.registry).await;
                }
            })
            .await;
    }

    /// Before any host connects: swept, and this ssh's options probed.
    async fn ready(&self) -> Result<&Features, String> {
        self.swept().await;
        self.features
            .get_or_try_init(|| probe(&self.ssh, &self.registry))
            .await
    }
}

impl Hosts {
    /// `dir` holds each host's control socket, forwarded socket and lock,
    /// and is made private; `ssh` is the program run.
    pub fn new(dir: Option<PathBuf>, ssh: PathBuf) -> Self {
        Self {
            env: Arc::new(Env {
                dir,
                ssh,
                registry: Registry::default(),
                swept: Default::default(),
                features: Default::default(),
            }),
            open: Default::default(),
        }
    }

    /// At launch, so a crash's leftovers do not wait for their host to be
    /// opened again.
    pub async fn prepare(&self) {
        self.env.swept().await;
    }

    /// What OpenSSH says `alias` connects to.
    pub fn resolve(&self, alias: String) -> impl Future<Output = Option<Value>> + Send + 'static {
        let env = self.env.clone();
        async move { resolve(&env.ssh, &alias, &env.registry).await }
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
                let dir = (self.env.dir.clone())
                    .ok_or("host_files_unusable: no HOME for ~/.agent/hosts")?;
                let host = Arc::new(Host {
                    alias: alias.to_owned(),
                    env: self.env.clone(),
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

    /// The app is quitting: every connection closes, all at once. Every ssh
    /// still running is asked to end first, so a host busy connecting gives
    /// up its link at once rather than at its own timeout. What has not
    /// ended by the deadline is killed; files a close could not remove are
    /// swept at the next launch.
    pub async fn close_all(&self) {
        let hosts: Vec<_> = self.open.lock().unwrap().values().cloned().collect();
        for host in &hosts {
            host.windows.store(0, Ordering::SeqCst);
        }
        self.env.registry.signal_all(libc::SIGTERM);
        let mut closing = tokio::task::JoinSet::new();
        for host in hosts {
            closing.spawn(async move { host.close().await });
        }
        let _ = tokio::time::timeout(QUIT_TIMEOUT, async {
            while closing.join_next().await.is_some() {}
        })
        .await;
        self.env.registry.signal_all(libc::SIGKILL);
    }
}

pub struct Host {
    pub alias: String,
    env: Arc<Env>,
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

/// A host's master: its process group, the end of what it printed, and
/// whether it was ended for printing too much.
struct Master {
    group: Group,
    stderr: Arc<std::sync::Mutex<String>>,
    flooded: Arc<AtomicBool>,
    reader: tokio::task::JoinHandle<()>,
}

impl Link {
    /// Whether the master still runs; one that exited takes its forward.
    /// One ended for flooding its stderr counts as a failure, so it is not
    /// started again at once.
    fn alive(&mut self, alias: &str) -> bool {
        let Some(master) = self.master.as_mut() else {
            self.forwarded = None;
            return false;
        };
        if !master.group.exited() {
            return true;
        }
        if master.flooded.load(Ordering::SeqCst) {
            self.failures += 1;
            self.failed = Some((
                Instant::now() + backoff(self.failures),
                flooded_reason(alias),
            ));
        }
        master.reader.abort();
        self.master = None;
        self.forwarded = None;
        false
    }
}

fn flooded_reason(alias: &str) -> String {
    format!(
        "host_output_too_large: ssh {alias} printed more than {MAX_OUTPUT} bytes of errors in one second"
    )
}

impl Host {
    /// The forwarded socket while the master runs and forwards it, so an
    /// attach tries it before asking the host anything.
    pub async fn reached(&self) -> Option<PathBuf> {
        let mut link = self.link.lock().await;
        (link.alive(&self.alias) && link.forwarded.is_some()).then(|| self.paths.socket.clone())
    }

    /// The remote home, once `agent start` has said it.
    pub async fn home(&self) -> Option<String> {
        let link = self.link.lock().await;
        link.remote.as_ref().and_then(|r| r.home.clone())
    }

    /// Nothing starts for a host no window has open: a window closed while
    /// an attach of its waited, or the app is quitting.
    fn still_open(&self) -> Result<(), String> {
        if self.windows.load(Ordering::SeqCst) == 0 {
            return Err(format!("host_closed: no window has {} open", self.alias));
        }
        Ok(())
    }

    /// Reach the host's daemon: the master connected (again, after it
    /// exited), `agent start` there, and its socket forwarded here. A
    /// failure stands, and is returned again, until its backoff has passed.
    pub async fn connect(&self) -> Result<PathBuf, String> {
        let features = self.env.ready().await?;
        let mut link = self.link.lock().await;
        self.still_open()?;
        if let Some((until, reason)) = &link.failed
            && Instant::now() < *until
        {
            return Err(reason.clone());
        }
        let result = self.establish(&mut link, features).await;
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

    async fn establish(&self, link: &mut Link, features: &Features) -> Result<PathBuf, String> {
        self.own(link)?;
        if !link.alive(&self.alias) {
            self.start_master(link, features).await?;
        }
        self.still_open()?;
        let output = self
            .run(&exec_args(
                &self.paths,
                &self.alias,
                &remote_command("start"),
                features,
            ))
            .await?;
        let remote = started(&self.alias, &output)?;
        if link.forwarded.as_deref() != Some(remote.socket.as_str()) {
            self.still_open()?;
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
        let features = self.env.ready().await?;
        let mut link = self.link.lock().await;
        self.still_open()?;
        link.failures = 0;
        link.failed = None;
        self.own(&mut link)?;
        if !link.alive(&self.alias) {
            self.start_master(&mut link, features).await?;
        }
        let output = self
            .run(&exec_args(
                &self.paths,
                &self.alias,
                &remote_command("shutdown"),
                features,
            ))
            .await?;
        match output.status.code() {
            Some(0) => Ok(()),
            Some(255) => Err(ssh_reason(
                &self.alias,
                &String::from_utf8_lossy(&output.stderr),
            )),
            Some(127) => Err(missing(&self.alias)),
            _ => Err(match crate::daemon::cli_reason(&output.stderr) {
                Some(reason) if reason.starts_with("daemon_unavailable") => return Ok(()),
                Some(reason) => format!("{reason} (on {})", self.alias),
                None => format!(
                    "daemon_stop_failed: agent shutdown on {} exited with {}",
                    self.alias, output.status
                ),
            }),
        }
    }

    /// No window needs the host any more: its master ends (closing the
    /// connection and the forward), and its files and lock go. The daemon
    /// there keeps running.
    pub async fn close(&self) {
        let mut link = self.link.lock().await;
        // A window opened again while this waited for the lock keeps it.
        if self.windows.load(Ordering::SeqCst) > 0 {
            return;
        }
        if let Some(mut master) = link.master.take() {
            master.group.end(STOP_GRACE).await;
            master.reader.abort();
        }
        link.forwarded = None;
        link.remote = None;
        link.failures = 0;
        link.failed = None;
        if let Some(lock) = link.lock.take() {
            remove_files(&self.paths, lock);
        }
    }

    /// Only one app process may own a host's files: another's master would
    /// have its forwarded socket unlinked from under it.
    fn own(&self, link: &mut Link) -> Result<(), String> {
        if link.lock.is_some() {
            return Ok(());
        }
        private_dir(&self.dir)?;
        link.lock = Some(lock_file(&self.paths.lock)?.ok_or_else(|| {
            format!(
                "host_busy: another Agent process has {} open; use its window or quit it",
                self.alias
            )
        })?);
        Ok(())
    }

    async fn start_master(&self, link: &mut Link, features: &Features) -> Result<(), String> {
        // A master an earlier run left is gone, control socket and all,
        // before its paths are used again.
        retire(&self.env.ssh, &self.env.registry, &self.paths, &self.alias).await?;
        let _ = std::fs::remove_file(&self.paths.socket);
        link.forwarded = None;
        let mut command = Command::new(&self.env.ssh);
        command
            .args(master_args(&self.paths, &self.alias, features))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut group = Group::spawn(&mut command, &self.env.registry)
            .map_err(|e| format!("host_unreachable: {}: {e}", self.env.ssh.display()))?;
        let stderr = Arc::new(std::sync::Mutex::new(String::new()));
        let flooded = Arc::new(AtomicBool::new(false));
        let reader = tokio::spawn(read_stderr(
            group.child.stderr.take().expect("piped"),
            stderr.clone(),
            flooded.clone(),
            self.env.registry.clone(),
            group.pgid,
        ));
        let mut master = Master {
            group,
            stderr,
            flooded,
            reader,
        };
        let deadline = Instant::now() + MASTER_TIMEOUT;
        loop {
            if std::fs::symlink_metadata(&self.paths.ctl).is_ok() {
                link.master = Some(master);
                return Ok(());
            }
            if master.group.exited() {
                // Everything it printed, before reading why.
                let _ = tokio::time::timeout(Duration::from_secs(1), &mut master.reader).await;
                if master.flooded.load(Ordering::SeqCst) {
                    return Err(flooded_reason(&self.alias));
                }
                let text = master.stderr.lock().unwrap().clone();
                return Err(ssh_reason(&self.alias, &text));
            }
            if Instant::now() > deadline {
                master.group.end(STOP_GRACE).await;
                master.reader.abort();
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

    /// Run ssh with its output bounded, in time and in bytes: a login
    /// shell that prints without end is killed, not buffered.
    async fn run(&self, args: &[OsString]) -> Result<Output, String> {
        bounded(
            &self.env.ssh,
            args,
            MAX_OUTPUT,
            COMMAND_TIMEOUT,
            &self.env.registry,
        )
        .await
        .map_err(|cut| match cut {
            Cut::Failed(e) => format!("host_unreachable: {}: {e}", self.env.ssh.display()),
            Cut::TimedOut => format!("host_timeout: ssh {} did not answer", self.alias),
            Cut::TooLarge => format!(
                "host_output_too_large: ssh {} printed more than {MAX_OUTPUT} bytes",
                self.alias
            ),
        })
    }
}

/// Keep the tail of stderr, stopping a sustained flood. Normal diagnostics
/// over a long connection do not consume a lifetime byte allowance.
async fn read_stderr(
    mut pipe: impl tokio::io::AsyncRead + Unpin,
    kept: Arc<std::sync::Mutex<String>>,
    flooded: Arc<AtomicBool>,
    registry: Registry,
    pgid: libc::pid_t,
) {
    use tokio::io::AsyncReadExt;
    let mut buffer = [0u8; 1024];
    let mut total = 0u64;
    let mut window = Instant::now();
    while let Ok(read) = pipe.read(&mut buffer).await {
        if read == 0 {
            break;
        }
        if window.elapsed() >= Duration::from_secs(1) {
            window = Instant::now();
            total = 0;
        }
        total += read as u64;
        if total > MAX_OUTPUT {
            flooded.store(true, Ordering::SeqCst);
            registry.signal(pgid, libc::SIGKILL);
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
}

/// What an earlier run of the app left, as after a crash: for each host
/// whose lock no process holds, a master still answering on its control
/// socket is retired, and its control socket, forwarded socket and lock are
/// removed. Nothing found is used.
async fn sweep(dir: &Path, ssh: &Path, registry: &Registry) {
    if !dir.is_dir() || private_dir(dir).is_err() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut stems: Vec<String> = entries
        .take(MAX_CONFIG_ENTRIES)
        .filter_map(|entry| {
            let name = entry.ok()?.file_name().into_string().ok()?;
            let (stem, kind) = name.rsplit_once('.')?;
            (stem.len() == 18 && stem.starts_with("h-") && matches!(kind, "lock" | "ctl" | "sock"))
                .then(|| stem.to_owned())
        })
        .collect();
    stems.sort();
    stems.dedup();
    let work = async {
        let mut jobs = tokio::task::JoinSet::new();
        for stem in stems {
            if jobs.len() == 8 {
                let _ = jobs.join_next().await;
            }
            let (files, ssh, registry) =
                (Paths::named(dir, &stem), ssh.to_owned(), registry.clone());
            jobs.spawn(async move {
                let Ok(Some(lock)) = lock_file(&files.lock) else {
                    return;
                };
                if retire(&ssh, &registry, &files, "stale").await.is_ok() {
                    remove_files(&files, lock);
                }
            });
        }
        while jobs.join_next().await.is_some() {}
    };
    // A slow stale host must not delay all windows. Unfinished retirement
    // leaves its files for that host's next connection to handle safely.
    let _ = tokio::time::timeout(RETIRE_TIMEOUT, work).await;
}

/// Retire a master an earlier run left on `files.ctl`: asked to exit, and
/// waited for until it has removed its control socket itself, as a master
/// does on exiting, so it cannot remove a new master's later. A socket
/// nothing answers on is only a file, and is removed.
async fn retire(ssh: &Path, registry: &Registry, files: &Paths, alias: &str) -> Result<(), String> {
    if std::fs::symlink_metadata(&files.ctl).is_err() {
        return Ok(());
    }
    let asked = bounded(
        ssh,
        &control_args(files, alias, "exit", None),
        MAX_OUTPUT,
        RETIRE_TIMEOUT,
        registry,
    )
    .await;
    let busy = || {
        format!(
            "host_busy: an ssh an earlier run of the app left for {alias} did not exit when asked"
        )
    };
    match asked {
        Ok(output) if output.status.success() => {
            let deadline = Instant::now() + RETIRE_TIMEOUT;
            while std::fs::symlink_metadata(&files.ctl).is_ok() {
                if Instant::now() > deadline {
                    return Err(busy());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Ok(())
        }
        Ok(_) => {
            let _ = std::fs::remove_file(&files.ctl);
            Ok(())
        }
        Err(_) => Err(busy()),
    }
}

/// Take a host's lock, or None while another process holds it. A lock file
/// removed and made again while this opened it is taken again, so two
/// processes never each hold a lock on a different file of one name.
fn lock_file(path: &Path) -> Result<Option<std::fs::File>, String> {
    use std::os::unix::{
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawFd,
    };
    let unusable = |e: std::io::Error| format!("host_files_unusable: {}: {e}", path.display());
    for _ in 0..8 {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(path)
            .map_err(unusable)?;
        // SAFETY: an advisory lock on a descriptor this function owns.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Ok(None);
        }
        let held = file.metadata().map_err(unusable)?;
        if std::fs::symlink_metadata(path)
            .is_ok_and(|now| now.dev() == held.dev() && now.ino() == held.ino())
        {
            return Ok(Some(file));
        }
    }
    Ok(None)
}

/// Remove a host's files, its lock last and while still held.
fn remove_files(paths: &Paths, lock: std::fs::File) {
    let _ = std::fs::remove_file(&paths.ctl);
    let _ = std::fs::remove_file(&paths.socket);
    let _ = std::fs::remove_file(&paths.lock);
    drop(lock);
}

/// What a command on a host may print on each of stdout and stderr, and a
/// master on stderr per second.
const MAX_OUTPUT: u64 = 1024 * 1024;

/// What `ssh -G` may print for one alias.
const MAX_RESOLVED: u64 = 64 * 1024;

/// The process groups the app has started and not yet reaped. A group is
/// signalled only while registered, and leaves before its leader is reaped,
/// so a registered id always names the app's own group.
#[derive(Clone, Default)]
struct Registry(Arc<std::sync::Mutex<HashSet<libc::pid_t>>>);

impl Registry {
    fn signal(&self, pgid: libc::pid_t, signal: libc::c_int) {
        let groups = self.0.lock().unwrap();
        if groups.contains(&pgid) {
            // SAFETY: a process group this app leads and has not reaped.
            unsafe { libc::kill(-pgid, signal) };
        }
    }

    fn signal_all(&self, signal: libc::c_int) {
        let groups = self.0.lock().unwrap();
        for pgid in groups.iter() {
            // SAFETY: as in `signal`.
            unsafe { libc::kill(-pgid, signal) };
        }
    }
}

/// An ssh the app started, leading a process group of its own, so what
/// OpenSSH starts under it ends with it. Its exit is found without reaping
/// it; then the rest of its group is killed, the group leaves the registry,
/// and only then is the leader reaped.
struct Group {
    child: Child,
    pgid: libc::pid_t,
    registry: Registry,
}

impl Group {
    fn spawn(command: &mut Command, registry: &Registry) -> std::io::Result<Self> {
        let mut groups = registry.0.lock().unwrap();
        let child = command.process_group(0).kill_on_drop(true).spawn()?;
        let pgid = (child.id().and_then(|pid| libc::pid_t::try_from(pid).ok()))
            .ok_or_else(|| std::io::Error::other("ssh started without a pid"))?;
        groups.insert(pgid);
        drop(groups);
        Ok(Self {
            child,
            pgid,
            registry: registry.clone(),
        })
    }

    fn signal(&self, signal: libc::c_int) {
        self.registry.signal(self.pgid, signal);
    }

    /// Whether the leader has exited; once it has, its group is ended and
    /// the leader reaped (see `Group`).
    fn exited(&mut self) -> bool {
        if self.child.id().is_none() {
            return true;
        }
        // SAFETY: a zeroed siginfo_t is valid; WNOWAIT only reports.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let found = unsafe {
            libc::waitid(
                libc::P_PID,
                self.pgid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if found == 0 && info.si_signo == 0 {
            return false;
        }
        if found != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            return false;
        }
        {
            let mut groups = self.registry.0.lock().unwrap();
            // SAFETY: the leader is not reaped, so the id is still its group's.
            unsafe { libc::kill(-self.pgid, libc::SIGKILL) };
            groups.remove(&self.pgid);
        }
        let _ = self.child.try_wait();
        true
    }

    async fn exit(&mut self) {
        let mut pause = Duration::from_millis(1);
        while !self.exited() {
            tokio::time::sleep(pause).await;
            pause = (pause * 2).min(Duration::from_millis(50));
        }
    }

    /// SIGTERM, so ssh closes its connection and removes its control
    /// socket, then SIGKILL after `grace`.
    async fn end(&mut self, grace: Duration) {
        self.signal(libc::SIGTERM);
        if tokio::time::timeout(grace, self.exit()).await.is_err() {
            self.signal(libc::SIGKILL);
            self.exit().await;
        }
    }
}

impl Drop for Group {
    /// Dropped before it ended (its owner was cancelled): killed, group and
    /// all; tokio then reaps the leader (`kill_on_drop`).
    fn drop(&mut self) {
        if self.child.id().is_some() {
            let mut groups = self.registry.0.lock().unwrap();
            // SAFETY: the leader is not reaped, so the id is still its group's.
            unsafe { libc::kill(-self.pgid, libc::SIGKILL) };
            groups.remove(&self.pgid);
        }
    }
}

/// Why a bounded ssh gave no output.
enum Cut {
    Failed(std::io::Error),
    TimedOut,
    TooLarge,
}

/// Run ssh as a `Group`, reading at most `limit` bytes from each of stdout
/// and stderr, within `within`. Past either bound its whole group is
/// killed. When it exits, the rest of its group is killed too, so a
/// descendant holding its pipes cannot keep this waiting.
async fn bounded(
    ssh: &Path,
    args: &[OsString],
    limit: u64,
    within: Duration,
    registry: &Registry,
) -> Result<Output, Cut> {
    let mut command = Command::new(ssh);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut group = Group::spawn(&mut command, registry).map_err(Cut::Failed)?;
    let (out, err) = (group.child.stdout.take(), group.child.stderr.take());
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let ran = tokio::time::timeout(within, async {
        tokio::try_join!(
            async {
                capped(out, limit, &mut stdout)
                    .await
                    .map_err(|()| Cut::TooLarge)
            },
            async {
                capped(err, limit, &mut stderr)
                    .await
                    .map_err(|()| Cut::TooLarge)
            },
            async {
                group.exit().await;
                Ok::<(), Cut>(())
            },
        )
    })
    .await;
    let cut = match ran {
        Ok(Ok(_)) => {
            let status = (group.child.try_wait().ok().flatten()).ok_or_else(|| {
                Cut::Failed(std::io::Error::other("ssh's exit was not collected"))
            })?;
            return Ok(Output {
                status,
                stdout,
                stderr,
            });
        }
        Ok(Err(cut)) => cut,
        Err(_) => Cut::TimedOut,
    };
    group.signal(libc::SIGKILL);
    group.exit().await;
    Err(cut)
}

/// Read a pipe to its end, refusing more than `limit` bytes.
async fn capped(
    pipe: Option<impl tokio::io::AsyncRead + Unpin>,
    limit: u64,
    into: &mut Vec<u8>,
) -> Result<(), ()> {
    use tokio::io::AsyncReadExt;
    let Some(pipe) = pipe else { return Ok(()) };
    // A read error ends the output; the exit status says the rest.
    let _ = pipe.take(limit + 1).read_to_end(into).await;
    if into.len() as u64 > limit {
        return Err(());
    }
    Ok(())
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
    -N|-T|-n) ;;
    -G) resolve=1 ;;
    -F) shift ;;
    -S) ctl=$2; shift ;;
    -o) [ "$2" = ControlMaster=yes ] && master=1; shift ;;
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
fwds="$root/fwd-$(basename "$ctl")"
cleanup() {{ [ -f "$fwds" ] && rm -f $(cat "$fwds") "$fwds"; rm -f "$ctl"; }}
if [ -n "$master" ]; then
  echo master >> "$root/log"
  [ "$mode" = auth ] && refuse
  [ "$mode" = noisy ] && exec yes 'channel error' >&2
  # As a real master: its control socket goes a moment after it is told
  # to exit, once its connection has closed.
  trap 'sleep 0.2; cleanup; exit 0' TERM INT HUP
  [ "$mode" = stubborn ] && trap '' TERM
  echo $$ > "$ctl"
  while [ -e "$ctl" ]; do sleep 0.05; done
  cleanup; exit 255
fi
if [ -n "$op" ]; then
  echo "$op" >> "$root/log"
  [ -e "$ctl" ] || {{ echo "Control socket connect($ctl): No such file or directory" >&2; exit 255; }}
  case "$op" in
    forward) ln -sfn "${{fwd#*:}}" "${{fwd%%:*}}" && echo "${{fwd%%:*}}" >> "$fwds" ;;
    cancel) rm -f "${{fwd%%:*}}" ;;
    exit) kill "$(cat "$ctl")" 2>/dev/null || {{ echo "Control socket connect($ctl): Connection refused" >&2; exit 255; }} ;;
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
                    "#!/bin/sh\necho \"agent $*\" >> '{r}/log'\n[ \"$1\" = start ] || exit 0\ncat '{r}/ready'\n[ {status} = 0 ] || echo '{{\"error\":\"daemon_protocol_mismatch\",\"detail\":\"the daemon speaks protocol 1\"}}' >&2\nexit {status}\n"
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
    /// The processes still running (zombies aside) whose command line names
    /// `needle`, from the process table.
    pub fn running(needle: &str) -> Vec<String> {
        let table = std::process::Command::new("ps")
            .args(["-ax", "-o", "pid=,stat=,command="])
            .output()
            .unwrap();
        String::from_utf8_lossy(&table.stdout)
            .lines()
            .filter(|line| line.contains(needle))
            .filter(|line| {
                !(line.split_whitespace().nth(1)).is_some_and(|stat| stat.starts_with('Z'))
            })
            .map(str::to_owned)
            .collect()
    }

    /// Whether process `pid` has ended (or is only a zombie).
    pub fn ended(pid: &str) -> bool {
        let table = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", pid.trim()])
            .output()
            .unwrap();
        let stat = String::from_utf8_lossy(&table.stdout);
        stat.trim().is_empty() || stat.trim().starts_with('Z')
    }

    /// Wait up to three seconds for `check`, then fail naming `what`.
    pub async fn eventually(what: &str, check: impl Fn() -> bool) {
        for _ in 0..150 {
            if check() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("{what}");
    }

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
            "# hosts\nInclude config.d/*.conf\nHost box build-1 *.internal !gone\n  HostName 10.0.0.5\n\
             Host=eq\nhost \"quoted\" # trailing\nHost ?x -oProxyCommand=x a/b\n\
             Host *\n  Include ~/other/extra missing\nMatch host box\n  Include ~/other/only-box\n\
             Host box\n  Include ~/other/only-box config\nMatch all\n  Include ~/other/all\n",
        )
        .unwrap();
        std::fs::write(home.join(".ssh/config.d/b.conf"), "Host second\n").unwrap();
        std::fs::write(home.join(".ssh/config.d/a.conf"), "Host first\n").unwrap();
        std::fs::write(home.join(".ssh/config.d/.hidden.conf"), "Host hidden\n").unwrap();
        std::fs::write(home.join(".ssh/config.d/c.txt"), "Host notconf\n").unwrap();
        std::fs::write(home.join("other/extra"), "Host extra\n").unwrap();
        std::fs::write(home.join("other/all"), "Host every\n").unwrap();
        // Only where it applies to one host: not followed.
        std::fs::write(home.join("other/only-box"), "Host conditional\n").unwrap();
        assert_eq!(
            aliases(&home.join(".ssh/config"), &home),
            [
                "first", "second", "box", "build-1", "eq", "quoted", "extra", "every"
            ]
        );
        assert!(aliases(&home.join("none"), &home).is_empty());
        // Many hosts, each named many times: listed once, in order, up to
        // the bound, in linear time.
        let many: String = (0..4 * MAX_ALIASES)
            .map(|n| format!("Host h{n} h{n} h0\n"))
            .collect();
        std::fs::write(home.join(".ssh/many"), many).unwrap();
        let listed = aliases(&home.join(".ssh/many"), &home);
        assert_eq!(listed.len(), MAX_ALIASES);
        assert_eq!(listed[..3], ["h0", "h1", "h2"]);
        assert_eq!(listed[MAX_ALIASES - 1], format!("h{}", MAX_ALIASES - 1));
        // Includes share one budget of bytes read.
        let big = format!("# {}\n", "x".repeat(MAX_CONFIG as usize - 16));
        for n in 0..5 {
            std::fs::write(
                home.join(format!(".ssh/big{n}")),
                format!("Host big{n}\n{big}"),
            )
            .unwrap();
        }
        std::fs::write(home.join(".ssh/bigs"), "Include big*\n").unwrap();
        let listed = aliases(&home.join(".ssh/bigs"), &home);
        // Four fit; the fifth is past the budget.
        assert_eq!(listed, ["big0", "big1", "big2", "big3"]);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn an_alias_is_one_concrete_destination_never_an_option() {
        for good in ["box", "build-1.example", "[::1]"] {
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
            // ssh reads this as user `user` at host `box`.
            "user@box",
        ] {
            assert!(!valid_alias(bad), "{bad:?}");
        }
        assert!(glob("*.conf", "a.conf") && glob("a?c*", "abcdef") && !glob("*.conf", "a.txt"));
    }

    #[test]
    fn ssh_is_asked_for_one_master_that_never_prompts_and_forwards_the_socket() {
        let dir = Path::new("/tmp/agent-hosts");
        let p = paths(dir, "box").unwrap();
        assert_eq!(p.ctl.parent(), Some(dir));
        let name = p.ctl.file_stem().unwrap().to_str().unwrap().to_owned();
        assert!(name.starts_with("h-") && name.len() == 18, "{name}");
        assert_eq!(p.socket, dir.join(format!("{name}.sock")));
        assert_eq!(p.lock, dir.join(format!("{name}.lock")));
        // Aliases differing only in case get their own files, even where
        // the file system does not tell `Box` from `box`.
        let upper = paths(dir, "Box").unwrap();
        assert_ne!(
            upper.ctl.to_str().unwrap().to_lowercase(),
            p.ctl.to_str().unwrap().to_lowercase()
        );
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
        // Every forced option, on the roles it is for, as `-o`; the
        // master has no session, and neither a prompt nor a fork.
        let all = Features::default();
        let master = text(master_args(&p, "box", &all));
        let exec = text(exec_args(&p, "box", "agent start", &all));
        for (option, for_master, for_exec, _) in FORCED {
            for (args, wanted) in [(&master, for_master), (&exec, for_exec)] {
                let at = args.iter().position(|a| a == option);
                assert_eq!(at.is_some(), *wanted, "{option}");
                if let Some(at) = at {
                    assert_eq!(args[at - 1], "-o");
                }
            }
        }
        assert_eq!(master[0], "-N");
        assert_eq!(
            &master[master.len() - 4..],
            ["-S", p.ctl.to_str().unwrap(), "--", "box"]
        );
        assert_eq!(
            &exec[exec.len() - 5..],
            ["-S", p.ctl.to_str().unwrap(), "--", "box", "agent start"]
        );
        // An option this ssh does not know is left out, not sent to fail.
        let old = Features {
            unknown: vec!["ForkAfterAuthentication=no", "SessionType=default"],
        };
        let exec_old = text(exec_args(&p, "box", "agent start", &old));
        assert!(
            !exec_old
                .iter()
                .any(|a| a.starts_with("ForkAfterAuthentication"))
        );
        assert!(!exec_old.iter().any(|a| a.starts_with("SessionType")));
        assert!(exec_old.contains(&"ControlPersist=no".to_owned()));
        // A request to the master reads no config.
        assert_eq!(
            text(control_args(
                &p,
                "box",
                "forward",
                Some("/home/u/.agent/state.sqlite.sock")
            )),
            [
                "-F",
                "none",
                "-S",
                p.ctl.to_str().unwrap(),
                "-O",
                "forward",
                "-L",
                format!("{}:/home/u/.agent/state.sqlite.sock", p.socket.display()).as_str(),
                "--",
                "box"
            ]
        );
        // ssh would expand these in a control path.
        for odd in ["/tmp/a%b", "/tmp/a$b"] {
            let refused = paths(Path::new(odd), "box").unwrap_err();
            assert!(refused.starts_with("host_path_unusable: "), "{refused}");
        }
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
                &output(2, "", r#"{"error":"usage","detail":"no provider: pass"}"#)
            )),
            "host_no_provider"
        );
        // The host's agent accepted an older daemon: that agent is old.
        assert_eq!(
            code(started("box", &output(0, &line(now - 1), ""))),
            "host_agent_older"
        );
        // It refused one: an upgrade there left the old daemon, which it can replace.
        let refused =
            r#"{"error":"daemon_protocol_mismatch","detail":"the daemon speaks protocol 3"}"#;
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
                &output(
                    1,
                    "",
                    r#"{"error":"daemon_start_failed","detail":"disk full"}"#
                )
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
        let files = paths(&shim.root.join("hosts"), "box").unwrap();
        assert_eq!(local, files.socket);
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
        let pid: i32 = std::fs::read_to_string(&files.ctl)
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
        assert!(!files.ctl.exists());
        assert!(std::fs::symlink_metadata(&files.socket).is_err());
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

    #[test]
    fn config_words_split_as_openssh_splits_them() {
        let split = |line: &str| words(line);
        assert_eq!(
            split("Host 'single' \"double\""),
            ["Host", "single", "double"]
        );
        // Quotes open mid-word and keep spaces; the other quote is literal.
        assert_eq!(split("Host a'b c'd \"it's\""), ["Host", "ab cd", "it's"]);
        // `\` escapes a quote, a backslash, or a space outside quotes, and
        // is kept before anything else.
        assert_eq!(
            split(r#"Host a\ b \'q\' "x\ y" c\d e\\f"#),
            ["Host", "a b", "'q'", r"x\ y", r"c\d", r"e\f"]
        );
        assert_eq!(split("Host=box\t# note"), ["Host", "box"]);
        assert_eq!(split("Host a#b #c"), ["Host", "a#b"]);
        // OpenSSH rejects a line with an open quote; it names no hosts.
        assert!(split("Host 'open box").is_empty());
        assert!(split("Host \"open").is_empty());
    }

    #[tokio::test]
    async fn resolving_an_alias_reads_what_ssh_says_and_no_more() {
        let shim = Shim::new("host-resolve");
        let registry = Registry::default();
        let answer = resolve(&shim.ssh, "box", &registry).await.unwrap();
        assert_eq!(
            answer,
            json!({"user": "someone", "hostname": "box.example", "port": "22"})
        );
        // What ssh started (a `Match exec`, say) ends with it.
        let flood = shim.root.join("bin/flood");
        super::shim::script(
            &flood,
            "#!/bin/sh\nsleep 30 &\necho $! > \"$0.pid\"\nexec yes user\n",
        );
        let begun = std::time::Instant::now();
        assert_eq!(resolve(&flood, "box", &registry).await, None);
        assert!(begun.elapsed() < std::time::Duration::from_secs(4));
        let pid = std::fs::read_to_string(shim.root.join("bin/flood.pid")).unwrap();
        super::shim::eventually("the probe's descendant ends", || super::shim::ended(&pid)).await;
        assert!(registry.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_ssh_that_does_not_finish_in_time_is_ended_with_what_it_started() {
        let shim = Shim::new("host-slow");
        let registry = Registry::default();
        let slow = shim.root.join("bin/slow");
        super::shim::script(
            &slow,
            "#!/bin/sh\nsleep 30 &\necho $! > \"$0.pid\"\nexec sleep 30\n",
        );
        let begun = std::time::Instant::now();
        let ran = bounded(&slow, &[], 1024, Duration::from_millis(300), &registry).await;
        assert!(matches!(ran, Err(Cut::TimedOut)));
        assert!(begun.elapsed() < Duration::from_secs(2));
        let pid = std::fs::read_to_string(shim.root.join("bin/slow.pid")).unwrap();
        super::shim::eventually("the timed-out ssh's descendant ends", || {
            super::shim::ended(&pid)
        })
        .await;
        // One that exits leaves nothing either: the rest of its group goes.
        let quick = shim.root.join("bin/quick");
        super::shim::script(
            &quick,
            "#!/bin/sh\nsleep 30 > /dev/null 2>&1 &\necho $! > \"$0.pid\"\necho done\n",
        );
        let ran = bounded(&quick, &[], 1024, Duration::from_secs(5), &registry)
            .await
            .ok()
            .unwrap();
        assert_eq!(ran.stdout, b"done\n");
        let pid = std::fs::read_to_string(shim.root.join("bin/quick.pid")).unwrap();
        super::shim::eventually("the exited ssh's descendant ends", || {
            super::shim::ended(&pid)
        })
        .await;
        assert!(registry.0.lock().unwrap().is_empty());
    }

    /// Every step of a host's connection, and after each, the process table
    /// and the hosts directory hold only what that step should leave.
    #[tokio::test]
    async fn a_hosts_connection_leaves_nothing_behind_on_any_path() {
        use super::shim::{eventually, running};
        let shim = Shim::new("host-life");
        let root = shim.root.to_str().unwrap().to_owned();
        let dir = shim.root.join("hosts");
        let files = paths(&dir, "box").unwrap();
        let socket = shim.remote.join("daemon.sock");
        daemon(&socket, "s1");
        shim.agent(&ready(&socket, agent_client::PROTOCOL), 0);
        let masters = || {
            (running(&root).into_iter())
                .filter(|line| line.contains("ControlMaster=yes"))
                .count()
        };
        let left = || {
            let mut names: Vec<String> = std::fs::read_dir(&dir)
                .map(|d| {
                    d.filter_map(|e| e.ok()?.file_name().into_string().ok())
                        .collect()
                })
                .unwrap_or_default();
            names.sort();
            names
        };
        let hosts = shim.hosts();
        let host = hosts.acquire("box").unwrap();
        // Open: one master, its sockets and lock.
        let local = host.connect().await.unwrap();
        agent_client::Client::connect(&local).await.unwrap();
        assert_eq!(masters(), 1);
        for file in [&files.ctl, &files.socket, &files.lock] {
            assert!(
                std::fs::symlink_metadata(file).is_ok(),
                "{}",
                file.display()
            );
        }
        // The master is killed from outside: the next attach starts another.
        let pid = std::fs::read_to_string(&files.ctl).unwrap();
        // SAFETY: the stand-in master this test started.
        unsafe { libc::kill(pid.trim().parse().unwrap(), libc::SIGKILL) };
        eventually("the dead master is noticed", || {
            futures_now(host.reached()).is_none()
        })
        .await;
        let local = host.connect().await.unwrap();
        agent_client::Client::connect(&local).await.unwrap();
        assert_eq!(masters(), 1);
        // The store is replaced there: the host's own shutdown, then another
        // store answers on the same socket, through the same master.
        host.replace().await.unwrap();
        std::fs::remove_file(&socket).unwrap();
        daemon(&socket, "s2");
        let local = host.connect().await.unwrap();
        let (client, _events) = agent_client::Client::connect(&local).await.unwrap();
        assert_eq!(client.store(), Some("s2"));
        assert_eq!(masters(), 1);
        drop(client);
        // The last window closes: no process, no file.
        hosts.release(&host).await;
        assert_eq!(running(&root), Vec::<String>::new());
        assert_eq!(left(), Vec::<String>::new());
        // An attach that was waiting when it closed starts nothing.
        let closed = host.connect().await.unwrap_err();
        assert!(closed.starts_with("host_closed: "), "{closed}");
        assert_eq!(running(&root), Vec::<String>::new());
        // Opened again, then the app quits.
        let host = hosts.acquire("box").unwrap();
        host.connect().await.unwrap();
        assert_eq!(masters(), 1);
        hosts.close_all().await;
        assert_eq!(running(&root), Vec::<String>::new());
        assert_eq!(left(), Vec::<String>::new());
        assert_eq!(shim.count("master"), 3);
    }

    /// Poll a future once: `reached` answers at once.
    fn futures_now<T>(future: impl Future<Output = T>) -> T {
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        let mut future = std::pin::pin!(future);
        match future.as_mut().poll(&mut context) {
            std::task::Poll::Ready(value) => value,
            std::task::Poll::Pending => panic!("pending"),
        }
    }

    /// A crash leaves a master connected and its files. The next launch
    /// retires that master, waiting until it has removed its own control
    /// socket, so it cannot remove the new master's, and uses nothing it left.
    #[tokio::test]
    async fn a_crash_leaves_nothing_the_next_launch_keeps() {
        use super::shim::{eventually, running};
        let shim = Shim::new("host-crash");
        let root = shim.root.to_str().unwrap().to_owned();
        let dir = shim.root.join("hosts");
        let files = paths(&dir, "box").unwrap();
        let socket = shim.remote.join("daemon.sock");
        daemon(&socket, "s1");
        shim.agent(&ready(&socket, agent_client::PROTOCOL), 0);
        // What the crashed run left: its master, still connected, in a group
        // of its own; its forwarded socket; its lock, held by no one.
        private_dir(&dir).unwrap();
        let mut orphan = std::process::Command::new(&shim.ssh);
        std::os::unix::process::CommandExt::process_group(&mut orphan, 0);
        let orphan = orphan
            .args(master_args(&files, "box", &Features::default()))
            .spawn()
            .unwrap();
        let old = orphan.id().to_string();
        eventually("the orphan master is up", || files.ctl.exists()).await;
        std::fs::write(&files.lock, "").unwrap();
        std::os::unix::fs::symlink("/nonexistent", &files.socket).unwrap();
        // The next launch.
        let hosts = shim.hosts();
        hosts.prepare().await;
        assert!(super::shim::ended(&old) || !running(&root).iter().any(|l| l.contains(&old)));
        assert!(std::fs::read_dir(&dir).unwrap().next().is_none());
        let host = hosts.acquire("box").unwrap();
        let local = host.connect().await.unwrap();
        // Past when the old master's cleanup would have run: the new one's
        // control socket and forward stand.
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(files.ctl.exists());
        agent_client::Client::connect(&local).await.unwrap();
        hosts.close_all().await;
        assert_eq!(running(&root), Vec::<String>::new());
        drop(orphan);
    }

    /// A config that says the opposite of everything the app forces, and
    /// sets up what the app clears.
    const HOSTILE: &str = "IgnoreUnknown ForkAfterAuthentication,SessionType,ChannelTimeout
Host *
  BatchMode no
  ConnectTimeout 1
  LogLevel QUIET
  RequestTTY force
  RemoteCommand echo hostile
  ForkAfterAuthentication yes
  ControlPersist yes
  ControlMaster autoask
  ControlPath /tmp/hostile-%C
  SessionType none
  ClearAllForwardings no
  LocalForward 5999 localhost:22
  RemoteForward 5998 localhost:22
  DynamicForward 1080
  Tunnel yes
  ForwardAgent yes
  ForwardX11 yes
  PermitLocalCommand yes
  LocalCommand touch /tmp/hostile-local
  AddKeysToAgent ask
  ServerAliveInterval 0
  ServerAliveCountMax 100
  ChannelTimeout global=10s direct-streamlocal@openssh.com=5s
  ExitOnForwardFailure no
  StreamLocalBindUnlink no
";

    /// Real OpenSSH (`ssh` on PATH): with the hostile config, what `ssh -G`
    /// says takes effect for every forced option is what it says with no
    /// config at all. `StreamLocalBindMask` is the one OpenSSH lets a config
    /// override (see `FORCED`), so it is not asserted here.
    #[test]
    fn discovery_never_invents_a_truncated_alias_or_expands_past_its_budget() {
        let shim = Shim::new("host-discovery-bounds");
        let config = shim.root.join("config");
        std::fs::write(&config, "Host complete\nHost production\n").unwrap();
        let mut found = Found {
            list: Vec::new(),
            seen: HashSet::new(),
            budget: 23,
            entries: 10,
        };
        read_config(&config, &shim.root, 0, &mut found);
        assert_eq!(found.list, ["complete"]);
        let dir = shim.root.join("includes");
        std::fs::create_dir(&dir).unwrap();
        for n in 0..10 {
            std::fs::write(dir.join(n.to_string()), "Host x").unwrap();
        }
        let mut budget = 3;
        assert_eq!(
            expand(&format!("{}/*", dir.display()), &shim.root, &mut budget).len(),
            3
        );
        assert_eq!(budget, 0);
        assert!(expand(&format!("{}/*", dir.display()), &shim.root, &mut budget).is_empty());
        assert!(paths(Path::new("/tmp/has:colon"), "box").is_err());
    }

    #[tokio::test]
    async fn failed_option_probes_are_not_cached_as_missing_safeguards() {
        let shim = Shim::new("host-probe-retry");
        let hosts = shim.hosts();
        super::shim::script(
            &shim.ssh,
            "#!/bin/sh\necho 'temporarily unavailable' >&2\nexit 255\n",
        );
        assert!(
            hosts
                .env
                .ready()
                .await
                .unwrap_err()
                .starts_with("host_probe_failed:")
        );
        super::shim::script(&shim.ssh, "#!/bin/sh\nexit 0\n");
        assert!(hosts.env.ready().await.unwrap().unknown.is_empty());
    }

    #[tokio::test]
    async fn stale_hosts_share_one_retirement_deadline() {
        let shim = Shim::new("host-sweep-deadline");
        let dir = shim.root.join("hosts");
        private_dir(&dir).unwrap();
        for n in 0..20 {
            std::fs::write(dir.join(format!("h-{n:016x}.ctl")), "stale").unwrap();
        }
        super::shim::script(&shim.ssh, "#!/bin/sh\nsleep 30\n");
        let registry = Registry::default();
        let start = Instant::now();
        sweep(&dir, &shim.ssh, &registry).await;
        assert!(start.elapsed() < RETIRE_TIMEOUT + Duration::from_secs(1));
        for _ in 0..100 {
            if registry.0.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(registry.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn no_ssh_config_changes_what_the_app_forces() {
        let shim = Shim::new("host-hostile");
        let hostile = shim.root.join("hostile");
        std::fs::write(&hostile, HOSTILE).unwrap();
        let ssh = Path::new("ssh");
        let features = probe(ssh, &Registry::default()).await.unwrap();
        let p = paths(&shim.root.join("hosts"), "box").unwrap();
        let effective = |config: &Path, args: &[OsString]| {
            let out = std::process::Command::new(ssh)
                .arg("-F")
                .arg(config)
                .arg("-G")
                .args(args)
                .output()
                .unwrap();
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            assert!(out.status.success(), "{stderr}");
            let mut said: HashMap<String, Vec<String>> = HashMap::new();
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                let (key, value) = line.split_once(' ').unwrap_or((line, ""));
                said.entry(key.to_owned())
                    .or_default()
                    .push(value.to_owned());
            }
            said
        };
        let none = Path::new("none");
        for (master, args) in [
            (true, master_args(&p, "box", &features)),
            (false, exec_args(&p, "box", "agent start", &features)),
        ] {
            let (hostile, plain) = (effective(&hostile, &args), effective(none, &args));
            let mut keys = vec![
                "controlpath",
                "localforward",
                "remoteforward",
                "dynamicforward",
            ];
            for (option, for_master, for_exec, _) in FORCED {
                if (if master { *for_master } else { *for_exec }) && features.knows(option) {
                    keys.push(option.split('=').next().unwrap());
                }
            }
            for key in keys {
                let key = key.to_ascii_lowercase();
                if key == "streamlocalbindmask" {
                    continue;
                }
                assert_eq!(
                    hostile.get(&key),
                    plain.get(&key),
                    "master: {master}, {key}"
                );
            }
        }
        // A request to the master names `-F none` first, so a config given
        // before it is not read.
        let control = control_args(&p, "box", "exit", None);
        assert_eq!(control[..2], ["-F", "none"]);
        let quiet = ["-F", "none", "--", "box"].map(OsString::from);
        assert_eq!(effective(&hostile, &quiet), effective(none, &quiet[2..]));
    }

    /// The same lifecycle against a real host, run by hand:
    /// `AGENT_TEST_SSH_HOST` names an alias whose login shell has this
    /// version's `agent` on its PATH. The test replaces that account's agent
    /// store (moved aside to `~/.agent/replaced-*`, not deleted), so it must
    /// be a disposable account. `AGENT_TEST_SSH` may name the ssh to run (a
    /// wrapper adding `-F` with a hostile config, say), and
    /// `AGENT_TEST_SSH_AFTER` a shell command run after each step with the
    /// step's name as `$1`, which must succeed. After each step, no process
    /// naming this test's hosts directory runs that the step should not
    /// leave.
    #[tokio::test]
    #[ignore = "needs AGENT_TEST_SSH_HOST, a disposable account running this agent"]
    async fn a_real_hosts_connection_leaves_nothing_behind() {
        use super::shim::{eventually, running};
        let alias = std::env::var("AGENT_TEST_SSH_HOST").expect("AGENT_TEST_SSH_HOST");
        let ssh = PathBuf::from(std::env::var_os("AGENT_TEST_SSH").unwrap_or("ssh".into()));
        let root = PathBuf::from(format!("/tmp/agent-app-real-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("hosts");
        let ours = dir.to_str().unwrap().to_owned();
        let files = paths(&dir, &alias).unwrap();
        let masters = || -> Vec<String> {
            (running(&ours).into_iter())
                .filter(|line| line.contains("ControlMaster=yes"))
                .collect()
        };
        let left = || std::fs::read_dir(&dir).map_or(0, |d| d.count());
        let after = |step: &str| {
            if let Ok(hook) = std::env::var("AGENT_TEST_SSH_AFTER") {
                let status = std::process::Command::new("sh")
                    .args(["-c", &hook, "hook", step])
                    .status()
                    .unwrap();
                assert!(status.success(), "after {step}");
            }
        };
        let hosts = Hosts::new(Some(dir.clone()), ssh.clone());
        // Open.
        let host = hosts.acquire(&alias).unwrap();
        let local = host.connect().await.unwrap();
        let (client, _events) = agent_client::Client::connect(&local).await.unwrap();
        let first = client.store().map(str::to_owned);
        drop(client);
        assert_eq!(masters().len(), 1, "{:?}", masters());
        after("open");
        // The master is killed from outside; the next attach starts another.
        let old = masters()[0].split_whitespace().next().unwrap().to_owned();
        // SAFETY: the master this test's Hosts started.
        unsafe { libc::kill(old.parse().unwrap(), libc::SIGKILL) };
        eventually("the dead master is noticed", || {
            futures_now(host.reached()).is_none()
        })
        .await;
        let local = host.connect().await.unwrap();
        agent_client::Client::connect(&local).await.unwrap();
        assert_eq!(masters().len(), 1, "{:?}", masters());
        assert!(super::shim::ended(&old));
        after("reattach");
        // The store is replaced there: the host's own shutdown, the store
        // moved aside, and the next attach meets a new one.
        host.replace().await.unwrap();
        let features = hosts.env.ready().await.unwrap();
        let moved = bounded(
            &ssh,
            &exec_args(
                &files,
                &alias,
                "mkdir -p ~/.agent/replaced-$$ && mv ~/.agent/state.sqlite* ~/.agent/replaced-$$/",
                features,
            ),
            MAX_OUTPUT,
            COMMAND_TIMEOUT,
            &hosts.env.registry,
        )
        .await
        .ok()
        .unwrap();
        assert!(
            moved.status.success(),
            "{}",
            String::from_utf8_lossy(&moved.stderr)
        );
        let local = host.connect().await.unwrap();
        let (client, _events) = agent_client::Client::connect(&local).await.unwrap();
        assert_ne!(client.store().map(str::to_owned), first);
        drop(client);
        assert_eq!(masters().len(), 1, "{:?}", masters());
        after("replaced");
        // The last window closes.
        hosts.release(&host).await;
        assert_eq!(running(&ours), Vec::<String>::new());
        assert_eq!(left(), 0);
        after("closed");
        // Opened again, then the app quits.
        let host = hosts.acquire(&alias).unwrap();
        host.connect().await.unwrap();
        hosts.close_all().await;
        assert_eq!(running(&ours), Vec::<String>::new());
        assert_eq!(left(), 0);
        after("quit");
        // A crash: a master left connected, its lock held by no one. The
        // next launch retires it and keeps nothing it left.
        let mut orphan = std::process::Command::new(&ssh);
        std::os::unix::process::CommandExt::process_group(&mut orphan, 0);
        let orphan = orphan
            .args(master_args(&files, &alias, features))
            .stdin(std::process::Stdio::null())
            .spawn()
            .unwrap();
        eventually("the orphan master connects", || files.ctl.exists()).await;
        std::fs::write(&files.lock, "").unwrap();
        let old = orphan.id().to_string();
        let hosts = Hosts::new(Some(dir.clone()), ssh.clone());
        hosts.prepare().await;
        eventually("the orphan master exits", || super::shim::ended(&old)).await;
        assert_eq!(left(), 0);
        let host = hosts.acquire(&alias).unwrap();
        let local = host.connect().await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(files.ctl.exists());
        agent_client::Client::connect(&local).await.unwrap();
        hosts.close_all().await;
        assert_eq!(running(&ours), Vec::<String>::new());
        assert_eq!(left(), 0);
        after("crash");
        drop(orphan);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn diagnostics_over_time_do_not_end_a_healthy_master() {
        use tokio::io::AsyncWriteExt;
        let (mut writer, reader) = tokio::io::duplex(8192);
        let kept = Arc::new(std::sync::Mutex::new(String::new()));
        let flooded = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(read_stderr(
            reader,
            kept.clone(),
            flooded.clone(),
            Registry::default(),
            0,
        ));
        let burst = vec![b'x'; MAX_OUTPUT as usize / 2 + 1];
        writer.write_all(&burst).await.unwrap();
        tokio::time::sleep(Duration::from_millis(1100)).await;
        writer.write_all(&burst).await.unwrap();
        drop(writer);
        task.await.unwrap();
        assert!(!flooded.load(Ordering::SeqCst));
        assert_eq!(kept.lock().unwrap().len(), STDERR_KEPT);
    }

    /// A master that prints errors without end is ended, group and all, not
    /// read forever.
    #[tokio::test]
    async fn a_master_that_prints_without_end_is_ended() {
        let shim = Shim::new("host-noisy");
        let root = shim.root.to_str().unwrap().to_owned();
        shim.mode("noisy");
        let hosts = shim.hosts();
        let host = hosts.acquire("box").unwrap();
        let noisy = host.connect().await.unwrap_err();
        assert!(
            noisy.starts_with("host_output_too_large: ssh box"),
            "{noisy}"
        );
        assert_eq!(super::shim::running(&root), Vec::<String>::new());
        hosts.close_all().await;
    }

    /// Quitting closes every host at once, under one deadline, even when
    /// each master ignores SIGTERM for its whole grace.
    #[tokio::test]
    async fn quitting_closes_every_host_at_once() {
        use super::shim::running;
        let shim = Shim::new("host-quit");
        let root = shim.root.to_str().unwrap().to_owned();
        let socket = shim.remote.join("daemon.sock");
        daemon(&socket, "s1");
        shim.agent(&ready(&socket, agent_client::PROTOCOL), 0);
        shim.mode("stubborn");
        let hosts = shim.hosts();
        for alias in ["box1", "box2", "box3"] {
            hosts.acquire(alias).unwrap().connect().await.unwrap();
        }
        assert_eq!(running(&root).len(), 3, "{:?}", running(&root));
        let begun = Instant::now();
        hosts.close_all().await;
        assert!(begun.elapsed() < QUIT_TIMEOUT, "{:?}", begun.elapsed());
        assert_eq!(running(&root), Vec::<String>::new());
        let left: Vec<_> = (std::fs::read_dir(shim.root.join("hosts")).unwrap())
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[tokio::test]
    async fn a_host_that_prints_without_end_is_cut_off_not_buffered() {
        let shim = Shim::new("host-flood");
        super::shim::script(&shim.remote.join("bin/agent"), "#!/bin/sh\nexec yes\n");
        let hosts = shim.hosts();
        let host = hosts.acquire("box").unwrap();
        let begun = std::time::Instant::now();
        let flood = host.connect().await.unwrap_err();
        assert!(
            flood.starts_with("host_output_too_large: ssh box printed more than 1048576 bytes"),
            "{flood}"
        );
        assert!(begun.elapsed() < std::time::Duration::from_secs(5));
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
