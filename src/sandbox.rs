//! Confining a bot's commands as Codex does: its three sandbox modes, its
//! writable roots with read-only folders inside them, bubblewrap on Linux
//! and Seatbelt on macOS. The bubblewrap arguments and the Seatbelt policy
//! are ported from Codex's `linux-sandbox` and `sandboxing` crates
//! (Apache-2.0; see NOTICE), and kept in step with them.
//!
//! A bot with a sandbox writes only its roots: its folder, the folders its
//! client named, and the temporary directories. Reading stays open but for
//! the paths the daemon keeps unreadable, as Codex's `deny_read`. Inside
//! each root, what git, agent configuration and AWS profiles run or trust
//! stays read-only, and so does the daemon's own store. Network is off
//! unless the bot's client turned it on.

use crate::{Result, fail_with};
use agent_client::files::real;
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

pub mod known;
pub mod proxy;

/// How a bot's commands are confined, by Codex's `sandbox_mode` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// Nothing may be written.
    ReadOnly,
    /// The bot's roots may be written.
    WorkspaceWrite,
    /// No sandbox.
    DangerFullAccess,
}

/// In a git folder given as a root, so a bot may commit, what git runs
/// stays read-only as it does under `.git`: its hooks, the config that can
/// name other hooks or programs, its submodules' git folders, which hold
/// their own, and `commondir`, which would have git take another folder's
/// hooks and config in its place. Missing hooks, submodules and `commondir`
/// are kept from being made. A git folder is known by its name, `.git` or
/// `NAME.git`, which the bot can't change, never by what is inside it,
/// which it can.
const GIT_PROTECTED: [&str; 5] = ["hooks", "config", "config.worktree", "modules", "commondir"];

/// A folder the bot may write, and what inside it stays read-only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Root {
    pub path: PathBuf,
    /// Existing paths inside the root that stay read-only.
    pub read_only: Vec<PathBuf>,
    /// Paths inside the root that don't exist and may not be made.
    pub missing: Vec<PathBuf>,
    /// Paths right under the root that don't exist and may not be left made:
    /// files git reads whenever they are there, so an empty folder held in
    /// their place would stop git. On Linux, removed when the command ends;
    /// elsewhere they are `missing`.
    pub left: Vec<PathBuf>,
    /// Other daemons' stores and sockets inside the root, and the files
    /// SQLite keeps beside each store: read-only while they are there, and
    /// not to be made where they are not ([`known`]). On Linux one that is
    /// gone by the time the command starts is skipped, as a daemon that
    /// stops takes its socket and SQLite its journal.
    #[serde(default)]
    pub others: Vec<PathBuf>,
    /// On Linux, the lock files of its `missing` placeholders, made as the
    /// root is read, off the runtime thread, so a command built before its
    /// hold still has bubblewrap hold them ([`locks`]).
    #[serde(default)]
    pub locks: Vec<PathBuf>,
    /// On Linux, folders inside one of `read_only` that it may write again:
    /// in a git folder it may commit to, the bot's own linked worktree's
    /// folder in `worktrees`, which stays read-only for every other
    /// worktree. Its `commondir` stays in `read_only`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub opened: Vec<PathBuf>,
}

/// What a sandboxed command may do: write its roots, and reach the network
/// when `network` is on. `lend` sends it as the daemon built it, all but
/// what keeps the bot's socket listening, which stays in the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    /// Shallowest first: a deeper root decides for what is inside it.
    pub roots: Vec<Root>,
    pub network: bool,
    /// The daemon's socket, which no command in the sandbox reaches
    /// directly.
    pub socket: Option<PathBuf>,
    /// The bot's own socket into the daemon, which answers it as that bot
    /// and only as far as a sandboxed bot may go; in the daemon socket's
    /// place, so the `agent` CLI finds it there too.
    pub capability: Option<PathBuf>,
    /// What keeps that socket listening: it lives while the policy, or a
    /// command's [`Hold`] of it, does.
    #[serde(skip)]
    pub listening: Option<Listening>,
    /// The agent binary a confined write runs in ([`WRITE`]).
    pub helper: Option<PathBuf>,
    /// The folders, or sockets, whose Unix sockets its commands reach with
    /// the network off, those its settings name. With the network on they
    /// reach every one, as Codex's full network does: MCPX's, Docker's or
    /// ssh-agent's, which the person runs to be used. Off, only these, as
    /// Codex's network off reaches none: a socket may be a program's that
    /// runs as the person, which runs what a command asks of it outside
    /// the sandbox. Never the daemon's nor another bot's, either way.
    pub sockets: Vec<PathBuf>,
    /// Folders whose Unix sockets its commands never reach, the network on
    /// or off, named or not ([`Daemon::walled`]).
    pub walled: Vec<PathBuf>,
    /// The other daemons' sockets, which its commands never reach, the
    /// network on or off, nor the folder of each one's bots' own sockets
    /// for its bots: each answers as the person, or as another
    /// daemon's bot ([`known`]).
    #[serde(default)]
    pub daemons: Vec<PathBuf>,
    /// On Linux, each of [`Daemon::owned`] that is here, and the person's
    /// copy of it, bound read-only in its place: `(copy, place)`.
    #[serde(default)]
    pub owned: Vec<(PathBuf, PathBuf)>,
    /// The folders its commands may not read, nor write, link to or
    /// rename, and the `read` tool doesn't read for it
    /// ([`Daemon::unreadable`]).
    pub unreadable: Vec<PathBuf>,
    /// With the network off, the hosts its commands still reach, by
    /// pattern, through the daemon's proxy ([`proxy`]): its bot's
    /// `domains`, as Codex's network proxy takes them.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub domains: BTreeMap<String, proxy::Access>,
}

use std::collections::BTreeMap;

/// The argument that makes the agent binary the bridge a command with
/// `domains` reaches the proxy by, on Linux: it listens on the sandbox's
/// own loopback, hands each connection to the proxy by the descriptor
/// [`PROXY_FD`] names, and runs the command that follows `--`, pointed at
/// it, exiting as it does.
pub const PROXY: &str = "--agent-run-as-proxy";

/// The variable naming the descriptor the bridge asks the proxy for
/// connections on.
pub const PROXY_FD: &str = "AGENT_PROXY_FD";

/// Point `command`, a command run under `policy`, at its proxy when its
/// domains name any, over any variable its bot's environment set: on
/// macOS nothing runs between the sandbox and the command to set them
/// last, and Seatbelt lets it reach the proxy alone, so an inherited
/// `NO_PROXY` would leave a client no way out. On Linux the bridge sets
/// them as the command starts.
pub fn through_proxy(policy: &Policy, command: &mut tokio::process::Command) -> Result<()> {
    #[cfg(target_os = "macos")]
    if !policy.domains.is_empty() {
        let port = loopback_proxy(&policy.domains)?;
        command.envs(proxy_env(&format!("http://127.0.0.1:{port}")));
        command.env_remove("NO_PROXY").env_remove("no_proxy");
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (policy, command);
    Ok(())
}

/// The variables that point a command's HTTP clients at `proxy`, as Codex
/// sets them.
pub fn proxy_env(proxy: &str) -> Vec<(&'static str, String)> {
    [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "WS_PROXY",
        "WSS_PROXY",
        "http_proxy",
        "https_proxy",
        "ws_proxy",
        "wss_proxy",
    ]
    .into_iter()
    .map(|name| (name, proxy.to_owned()))
    .collect()
}

impl Policy {
    /// The socket a command in this sandbox reaches the daemon by: its
    /// bot's own, which bubblewrap puts in the daemon's socket's place, as
    /// Seatbelt can't.
    pub fn reach(&self) -> Option<&Path> {
        self.capability.as_ref()?;
        if cfg!(target_os = "linux") {
            self.socket.as_deref()
        } else {
            self.capability.as_deref()
        }
    }

    /// The policy for a bot in `mode`, whose folder is `workspace` and
    /// whose client named `writable`; `None` when its commands run
    /// unconfined. `daemon` holds what the daemon itself keeps: its store,
    /// which no root may write, and its socket.
    pub fn new(
        mode: Option<Mode>,
        workspace: Option<&Path>,
        writable: &[String],
        network: bool,
        daemon: &Daemon,
    ) -> Result<Option<Policy>> {
        let mode = match mode {
            None | Some(Mode::DangerFullAccess) => return Ok(None),
            Some(mode) => mode,
        };
        let mut paths = writable_paths(mode, workspace, writable);
        paths.sort_by_key(|path| (depth(path), path.clone()));
        paths.dedup();
        let others = match &daemon.known {
            Some(folder) => known::others(folder, daemon.store.as_deref()),
            None => Default::default(),
        };
        let mut daemons: Vec<PathBuf> = (others.iter())
            .filter_map(|other| other.socket.clone())
            .filter(|socket| Some(socket) != daemon.socket.as_ref())
            .collect();
        daemons.dedup();
        // The folder its own linked worktree keeps in its repository's git
        // folder, which it writes as it commits: of the checkout its folder
        // is in, as git finds it, the nearest `.git` above.
        let own = (workspace.into_iter())
            .flat_map(Path::ancestors)
            .map(|folder| folder.join(".git"))
            .find_map(|dot| {
                std::fs::symlink_metadata(&dot)
                    .ok()
                    .map(|found| (dot, found))
            })
            .filter(|(_, found)| found.is_file())
            .and_then(|(file, _)| gitdir(&file));
        let mut roots = Vec::new();
        for path in paths {
            // One gone since it was named is skipped, as Codex skips it:
            // bubblewrap can't bind what isn't there, and would refuse
            // every command until the bot's folders were named again.
            if std::fs::symlink_metadata(&path)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            {
                continue;
            }
            roots.push(root(&path, daemon, &others, own.as_deref())?);
        }
        Ok(Some(Policy {
            roots,
            network: network && mode == Mode::WorkspaceWrite,
            socket: daemon.socket.clone(),
            capability: None,
            listening: None,
            helper: daemon.helper.clone(),
            sockets: Vec::new(),
            walled: daemon.walled.clone(),
            // None that holds or is in a folder it may not read: its copy
            // would show what the folder hides, at a name of its own.
            owned: owned(
                &(daemon.owned.iter())
                    .filter(|place| {
                        !(daemon.unreadable.iter())
                            .any(|hidden| hidden.starts_with(place) || place.starts_with(hidden))
                    })
                    .cloned()
                    .collect::<Vec<_>>(),
            )
            .into_iter()
            .chain(
                (daemon.copied_files.iter())
                    .filter_map(|(place, first)| file_copy(place, first, &daemon.unreadable)),
            )
            .collect(),
            daemons,
            // One under another is masked with it: a mask under one
            // already closed would have nowhere to go.
            unreadable: (daemon.unreadable.iter())
                .filter(|hidden| {
                    !(daemon.unreadable.iter())
                        .any(|other| other != *hidden && hidden.starts_with(other))
                })
                .fold(Vec::new(), |mut kept, hidden| {
                    if !kept.contains(hidden) {
                        kept.push(hidden.clone());
                    }
                    kept
                }),
            domains: BTreeMap::new(),
        }))
    }

    /// This policy, its commands also reaching the Unix sockets at or under
    /// `paths`, absolute paths its settings name.
    /// What a sandboxed bot's `read` keeps to when no command sandbox is
    /// built for it, as the daemon reads for it outside any: `daemon`'s
    /// folders no sandboxed bot reads, and nothing else. A command run in it
    /// would write nothing and reach no network.
    pub fn reads(daemon: &Daemon) -> Policy {
        Policy {
            roots: Vec::new(),
            network: false,
            socket: None,
            capability: None,
            listening: None,
            helper: None,
            sockets: Vec::new(),
            walled: Vec::new(),
            daemons: Vec::new(),
            owned: Vec::new(),
            unreadable: daemon.unreadable.clone(),
            domains: BTreeMap::new(),
        }
    }

    pub fn with_sockets(mut self, paths: &[String]) -> Policy {
        self.sockets.extend(paths.iter().map(PathBuf::from));
        self
    }

    /// This policy, its commands reaching the network only at the hosts
    /// `domains` let through, by the proxy, when it reaches the network
    /// at all: with none named, every host, as before.
    pub fn with_domains(mut self, domains: &BTreeMap<String, proxy::Access>) -> Policy {
        if self.network && !domains.is_empty() {
            self.network = false;
            self.domains = domains.clone();
        }
        self
    }

    /// Whether a command in this sandbox may write `path`, an absolute
    /// path: the deepest root that holds it decides, as the sandbox's
    /// mounts and rules do. Symlinks in what exists of it are followed, as
    /// a write would follow them.
    pub fn can_write(&self, path: &Path) -> bool {
        real(path).is_some_and(|path| self.writes(&path))
    }

    /// Whether a command in this sandbox may read `path`, an absolute path:
    /// all but what is kept unreadable and what is under it, by the real
    /// path a read follows links to.
    pub fn can_read(&self, path: &Path) -> bool {
        real(path).is_some_and(|path| !self.hides(&path))
    }

    /// Whether `path`, a real path, is kept unreadable, or is under what is.
    fn hides(&self, path: &Path) -> bool {
        self.unreadable
            .iter()
            .any(|hidden| path.starts_with(hidden))
    }

    /// Whether a command in this sandbox may remove `path`, an absolute
    /// path: the name itself in the folder that holds it, so a link is
    /// judged where it lies, as removing it would remove it there, not by
    /// what it points to.
    pub fn can_remove(&self, path: &Path) -> bool {
        let (Some(folder), Some(name)) = (path.parent(), path.file_name()) else {
            return false;
        };
        real(folder).is_some_and(|folder| self.writes(&folder.join(name)))
    }

    /// Whether `path`, a real path, is not kept unreadable and the deepest
    /// root that holds it lets it be written.
    fn writes(&self, path: &Path) -> bool {
        !self.hides(path)
            && self
                .roots
                .iter()
                .rev()
                .find(|root| path.starts_with(&root.path))
                .is_some_and(|root| {
                    // The deepest of what it keeps and what it opens again
                    // inside that decides, as the mounts do.
                    let kept = (root.read_only.iter())
                        .chain(&root.missing)
                        .chain(&root.left)
                        .chain(&root.others)
                        .filter(|kept| path.starts_with(kept))
                        .map(|kept| depth(kept))
                        .max();
                    let opened = (root.opened.iter())
                        .filter(|opened| path.starts_with(opened))
                        .map(|opened| depth(opened))
                        .max();
                    kept.is_none_or(|kept| opened.is_some_and(|opened| opened > kept))
                })
    }
}

/// The folders a bot in `mode` may write: none when read-only; when
/// workspace-write, its folder, the folders its client named, and the
/// temporary directories, as Codex adds them, each by its real path, which
/// is what the sandbox sees.
fn writable_paths(mode: Mode, workspace: Option<&Path>, writable: &[String]) -> Vec<PathBuf> {
    match mode {
        Mode::ReadOnly | Mode::DangerFullAccess => Vec::new(),
        Mode::WorkspaceWrite => workspace
            .map(Path::to_owned)
            .into_iter()
            .chain(writable.iter().map(PathBuf::from))
            .chain(temporary().iter().map(|(_, real)| real.clone()))
            .collect(),
    }
}

/// `$TMPDIR` and `/tmp`, each as named and by its real path, read once:
/// what decides whether a call escapes must not change while a round runs.
fn temporary() -> &'static [(PathBuf, PathBuf)] {
    static TEMPORARY: std::sync::OnceLock<Vec<(PathBuf, PathBuf)>> = std::sync::OnceLock::new();
    TEMPORARY.get_or_init(|| {
        std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .into_iter()
            .chain([PathBuf::from("/tmp")])
            .filter_map(|path| std::fs::canonicalize(&path).ok().map(|real| (path, real)))
            .collect()
    })
}

/// The room left on the file system holding a temporary folder every
/// agent's commands share: bytes an unprivileged process may still write
/// and inodes it may still make, beside the totals. A file system that
/// keeps no inode count (Btrfs, ZFS and APFS report none, or as many as
/// are free) shows it with a zero total or equal figures.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Room {
    pub path: PathBuf,
    pub bytes_free: u64,
    pub bytes_total: u64,
    pub inodes_free: u64,
    pub inodes_total: u64,
}

/// [`Room`] on each temporary folder a sandboxed command may write
/// (`$TMPDIR`, then `/tmp`), as named, each file system's folder once by
/// its real path: one `statvfs` each, read now, so a caller runs it off
/// the runtime thread.
pub fn temporary_room() -> Vec<Room> {
    let mut seen: Vec<&Path> = Vec::new();
    let mut rooms = Vec::new();
    for (named, real) in temporary() {
        if seen.contains(&real.as_path()) {
            continue;
        }
        seen.push(real);
        if let Some(room) = room(named) {
            rooms.push(room);
        }
    }
    rooms
}

fn room(path: &Path) -> Option<Room> {
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: a zeroed statvfs is valid; statvfs fills it.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: a NUL-terminated path and a valid pointer to a statvfs this
    // function owns.
    if unsafe { libc::statvfs(name.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    let unit = stat.f_frsize as u64;
    #[allow(clippy::unnecessary_cast)]
    Some(Room {
        path: path.to_owned(),
        bytes_free: (stat.f_bavail as u64).saturating_mul(unit),
        bytes_total: (stat.f_blocks as u64).saturating_mul(unit),
        inodes_free: stat.f_favail as u64,
        inodes_total: stat.f_files as u64,
    })
}

/// Read what the escape check reads once, and make the placeholders' lock
/// folder, at start, rather than on the runtime thread when the first call
/// comes.
pub fn warm() {
    temporary();
    locks();
}

impl Mode {
    /// The stricter of two modes; no sandbox is the least strict.
    pub fn stricter(this: Option<Mode>, other: Option<Mode>) -> Option<Mode> {
        let rank = |mode: Option<Mode>| match mode {
            Some(Mode::ReadOnly) => 0,
            Some(Mode::WorkspaceWrite) => 1,
            Some(Mode::DangerFullAccess) | None => 2,
        };
        match rank(this) <= rank(other) {
            true => this,
            false => other,
        }
    }
}

/// The argument that makes the agent binary a confined write: it reads
/// the content on stdin and writes it to the path that follows, inside the
/// bot's sandbox, as Codex runs `apply_patch` inside its own. The kernel
/// then decides what the write reaches, so a link swapped in while it runs
/// can't carry it out.
pub const WRITE: &str = "--agent-run-as-write";

/// The argument that makes the agent binary apply a patch's changes, read
/// as JSON on stdin ([`Change`]), in order, inside the bot's sandbox: its
/// writes, removals and kept permissions, all in one run, so nothing the
/// patch does is done by the daemon outside the sandbox, as Codex runs
/// `apply_patch` inside its own.
pub const PATCH: &str = "--agent-run-as-patch";

/// One change [`PATCH`] makes.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Change {
    /// A file's new contents, and for a move the permission bits of the
    /// file it came from, set on the file opened.
    Put {
        path: PathBuf,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<u32>,
    },
    /// A name taken away, a link itself rather than what it names.
    Remove { path: PathBuf },
}

/// The confined patch [`PATCH`] asks for, run inside the sandbox.
pub fn patch() -> Result<()> {
    use std::io::Read;
    let mut given = Vec::new();
    std::io::stdin().read_to_end(&mut given)?;
    let changes: Vec<Change> = serde_json::from_slice(&given)
        .map_err(|error| crate::Error::with("invalid_patch", error.to_string()))?;
    for change in changes {
        match change {
            Change::Put {
                path,
                content,
                mode,
            } => put(&path, content.as_bytes(), mode)?,
            Change::Remove { path } => {
                std::fs::remove_file(&path).map_err(|error| denied(&path, error))?
            }
        }
    }
    Ok(())
}

fn denied(path: &Path, error: std::io::Error) -> crate::Error {
    match error.raw_os_error() {
        Some(libc::EACCES | libc::EPERM | libc::EROFS) => {
            crate::Error::with("write_denied", path.display().to_string())
        }
        _ => crate::Error::with("io_error", error.to_string()),
    }
}

/// The bridge [`PROXY`] asks for, run inside the sandbox: `args` is `--`
/// and the command, which it runs pointed at a port on the sandbox's own
/// loopback whose every connection goes to the proxy, and exits as it
/// does. Loopback stays the sandbox's own, reached directly
/// (`NO_PROXY`).
#[cfg(target_os = "linux")]
pub fn bridge(args: &[String]) -> Result<i32> {
    use std::os::unix::process::ExitStatusExt;
    let [dashes, program, rest @ ..] = args else {
        return crate::fail_with("usage", format!("{PROXY} -- PROGRAM [ARGS]..."));
    };
    if dashes != "--" {
        return crate::fail_with("usage", format!("{PROXY} -- PROGRAM [ARGS]..."));
    }
    let fd = (std::env::var(PROXY_FD).ok())
        .and_then(|fd| fd.parse::<i32>().ok())
        .ok_or_else(|| crate::Error::with("usage", format!("{PROXY} needs {PROXY_FD}")))?;
    // The command's way out is the port, not the descriptor.
    // SAFETY: fcntl on a descriptor number, which fails if it isn't open.
    unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
    let port = listener.local_addr()?.port();
    std::thread::Builder::new()
        .name("proxy-bridge".into())
        .spawn(move || {
            for client in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    if let Ok(proxy) = agent_client::socket::ask(fd) {
                        join(client, proxy);
                    }
                });
            }
        })?;
    let proxy = format!("http://127.0.0.1:{port}");
    let status = std::process::Command::new(program)
        .args(rest)
        .env_remove(PROXY_FD)
        .envs(proxy_env(&proxy))
        .env("NO_PROXY", "localhost,127.0.0.1,::1")
        .env("no_proxy", "localhost,127.0.0.1,::1")
        .status()?;
    if let Some(signal) = status.signal() {
        // Ended by a signal, as the command was.
        // SAFETY: signal and kill on this process alone.
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
            libc::kill(libc::getpid(), signal);
        }
        return Ok(128 + signal);
    }
    Ok(status.code().unwrap_or(1))
}

#[cfg(not(target_os = "linux"))]
pub fn bridge(_args: &[String]) -> Result<i32> {
    crate::fail_with("sandbox_unavailable", "the proxy bridge runs on Linux")
}

/// Copy each way between a command's connection and the proxy's until
/// both ends close.
#[cfg(target_os = "linux")]
fn join(client: std::net::TcpStream, proxy: std::os::unix::net::UnixStream) {
    let (Ok(mut client_in), Ok(mut proxy_in)) = (client.try_clone(), proxy.try_clone()) else {
        return;
    };
    let (mut client_out, mut proxy_out) = (client, proxy);
    let up = std::thread::spawn(move || {
        let _ = std::io::copy(&mut client_in, &mut proxy_out);
        let _ = proxy_out.shutdown(std::net::Shutdown::Write);
    });
    let _ = std::io::copy(&mut proxy_in, &mut client_out);
    let _ = client_out.shutdown(std::net::Shutdown::Write);
    let _ = up.join();
}

/// The confined write [`WRITE`] asks for, run inside the sandbox: make the
/// folders above `path`, then replace its content with stdin's. Only a
/// regular file is written.
pub fn write(path: &Path) -> Result<()> {
    use std::io::Read;
    let mut content = Vec::new();
    std::io::stdin().read_to_end(&mut content)?;
    put(path, &content, None)
}

/// Make the folders above `path`, then replace its content with
/// `content`, and its permission bits with `mode`, on the file opened.
fn put(path: &Path, content: &[u8], mode: Option<u32>) -> Result<()> {
    use std::io::Write;
    let denied = |error: std::io::Error| denied(path, error);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(denied)?;
    }
    match std::fs::metadata(path) {
        Ok(found) if !found.is_file() => return crate::fail("file_not_regular"),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(denied(error)),
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true);
    // As the daemon's own write: a FIFO swapped in must not block the open.
    std::os::unix::fs::OpenOptionsExt::custom_flags(
        &mut options,
        libc::O_NONBLOCK | libc::O_NOCTTY,
    );
    let mut file = options.open(path).map_err(denied)?;
    if !file.metadata()?.is_file() {
        return crate::fail("file_not_regular");
    }
    file.set_len(0).map_err(denied)?;
    file.write_all(content).map_err(denied)?;
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(mode))
            .map_err(denied)?;
    }
    Ok(())
}

/// The tools a sandbox confines: what runs a command or writes a file, and
/// input typed into a terminal, whose command runs in the sandbox.
pub fn confinable(tool: &str) -> bool {
    matches!(
        tool,
        "shell" | "write" | "edit" | "apply_patch" | "write_stdin"
    )
}

/// Whether a call by a bot in `mode` asks to run outside its sandbox: a
/// command that asks to escalate, or a write, edit or patch of a path
/// outside every folder the bot may write. `root` is the bot's folder and `folder`
/// the turn's, against which a relative path is read. Decided from the call
/// and the folders' names, with only the links the bot can't change
/// followed as [`learn`] read them for this call (`links`, carried with
/// it), so the round that announces a call, the store that records it and
/// the run that starts it agree; a confined write is still checked against
/// the real path when it runs.
pub fn escapes(
    mode: Mode,
    root: Option<&Path>,
    writable: &[String],
    tool: &str,
    arguments: &str,
    folder: &Path,
    links: &Links,
) -> bool {
    let Ok(arguments) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return false;
    };
    if tool == "shell" {
        return arguments["sandbox_permissions"] == "require_escalated";
    }
    let writable = writable_paths(mode, root, writable);
    // A patch escapes when any file it changes would.
    any_written(tool, &arguments, |path, removed| {
        let named = lexical(&folder.join(path));
        // A path [`learn`] did not follow is judged as spelled.
        let escapes = |removed| {
            let target = links.target(path, removed).unwrap_or(&named);
            outside(mode, &writable, &named, target)
        };
        // A removal takes the entry, but the tool also checks what it
        // leads to before running it, so it escapes when either is
        // outside.
        escapes(removed) || (removed && escapes(false))
    })
}

/// Whether `test` holds for any path a write, edit or patch call writes
/// or removes, told which: a removed one is the folder's entry, never what
/// a link there leads to.
fn any_written(
    tool: &str,
    arguments: &serde_json::Value,
    mut test: impl FnMut(&str, bool) -> bool,
) -> bool {
    match tool {
        "write" | "edit" => arguments["path"]
            .as_str()
            .is_some_and(|path| test(path, false)),
        "apply_patch" => arguments["input"]
            .as_str()
            .and_then(|input| crate::tools::Patch::parse(input).ok())
            .is_some_and(|patch| {
                patch.written().any(|path| test(path, false))
                    || patch.removed().any(|path| test(path, true))
            }),
        _ => false,
    }
}

/// Where a write, edit or patch call's paths lead, each with the links
/// in it followed whose folders the bot can't change ([`judged`]), by the
/// path as the call gives it and whether the call removes it. Read once
/// by [`learn`], off the runtime thread, and carried with the call, so
/// every judgment of it reads no file, looks up one entry per path, and
/// agrees; a path not in it (unread, or a link that couldn't be told) is
/// judged as spelled. Kept with a call that parks, so it resumes judged
/// as it was announced. One entry per path the call names, so it is no
/// bigger than the call. Calls are equal whatever was read for them.
#[derive(Clone, Debug, Default)]
pub struct Links(Option<std::sync::Arc<std::collections::HashMap<(String, bool), PathBuf>>>);

/// [`Links`] as kept: each path, whether it is removed, and where it
/// leads. [`learn`] keeps no target that isn't UTF-8, so none is lost.
type KeptLinks = Option<Vec<(String, bool, String)>>;

impl serde::Serialize for Links {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        let kept: KeptLinks = self.0.as_deref().map(|links| {
            links
                .iter()
                .filter_map(|((path, removed), to)| {
                    Some((path.clone(), *removed, to.to_str()?.to_owned()))
                })
                .collect()
        });
        kept.serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for Links {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let kept = <KeptLinks as serde::Deserialize>::deserialize(deserializer)?;
        Ok(Links(kept.map(|links| {
            std::sync::Arc::new(
                links
                    .into_iter()
                    .map(|(path, removed, to)| ((path, removed), PathBuf::from(to)))
                    .collect(),
            )
        })))
    }
}

impl PartialEq for Links {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for Links {}

impl Links {
    /// Whether [`learn`] read them.
    pub fn read(&self) -> bool {
        self.0.is_some()
    }

    /// Whether [`learn`] has not read them: nothing to keep.
    pub fn unread(&self) -> bool {
        self.0.is_none()
    }

    /// Where `path`, as the call gives it, leads, if [`learn`] read it.
    fn target(&self, path: &str, removed: bool) -> Option<&PathBuf> {
        self.0.as_deref()?.get(&(path.to_owned(), removed))
    }
}

/// Read where a call's paths lead, `tool` with `arguments`, for a bot in
/// `mode`: each link followed in the path as given, so a `..` after one
/// leaves where it leads, as the kernel reads it. Blocks: run it off the
/// runtime thread.
pub fn learn(
    mode: Mode,
    root: Option<&Path>,
    writable: &[String],
    tool: &str,
    arguments: &str,
    folder: &Path,
) -> Links {
    let writable = writable_paths(mode, root, writable);
    let mut links = std::collections::HashMap::new();
    if let Ok(arguments) = serde_json::from_str::<serde_json::Value>(arguments) {
        any_written(tool, &arguments, |path, removed| {
            // A removal is judged as the entry and as what it leads to.
            let ways: &[bool] = if removed { &[true, false] } else { &[false] };
            for &removed in ways {
                let read = |path: &Path| Some(read_link(path));
                let target = judgeable(path)
                    .then(|| {
                        let changeable = |dir: &Path| changeable(mode, &writable, dir);
                        judged(&folder.join(path), changeable, read, removed)
                    })
                    .flatten()
                    // Kept only as a name a parked call can keep whole, so
                    // the call is judged the same once it resumes.
                    .filter(|target| target.to_str().is_some());
                if let Some(target) = target {
                    links.insert((path.to_owned(), removed), target);
                }
            }
            false
        });
    }
    Links(Some(std::sync::Arc::new(links)))
}

/// What the link at `path` names; `None` when it is no link or can't be
/// read.
fn read_link(path: &Path) -> Option<PathBuf> {
    std::fs::symlink_metadata(path)
        .is_ok_and(|meta| meta.file_type().is_symlink())
        .then(|| std::fs::read_link(path).ok())
        .flatten()
}

/// Whether a write's path is one whose links are followed: one a write
/// could name, of up to 4096 bytes (as the tools take) with no NUL byte.
/// Any other is judged as spelled, and the tool refuses it anyway.
fn judgeable(path: &str) -> bool {
    path.len() <= 4096 && !path.contains('\0')
}

/// Whether a path, `named` as spelled and `target` with links followed,
/// lies outside every folder a bot in `mode` may write.
fn outside(mode: Mode, writable: &[PathBuf], named: &Path, target: &Path) -> bool {
    // Inside as spelled or as followed: a folder may be given by a link.
    let inside = |dir: &Path| named.starts_with(dir) || target.starts_with(dir);
    let temporary = temporary()
        .iter()
        .any(|(named, real)| mode == Mode::WorkspaceWrite && (inside(named) || inside(real)));
    !(temporary || writable.iter().any(|dir| inside(dir)))
}

/// Whether a bot that writes `writable` may change what is in `dir`.
fn changeable(mode: Mode, writable: &[PathBuf], dir: &Path) -> bool {
    writable.iter().any(|root| dir.starts_with(root))
        || (mode == Mode::WorkspaceWrite
            && temporary().iter().any(|(named, _)| dir.starts_with(named)))
}

/// `path` with each link in it followed whose folder the bot can't
/// write (`changeable` false), as a home or system folder reached through
/// a link: such a link stays as it is between the round that judges a
/// call and the run that starts it. A link in a folder the bot writes is
/// left as spelled, as Codex substitutes no mutable link's target. Each
/// target is read by name, as the path itself is. `link` says what a path
/// links to, `Some(None)` for no link; `None` when it can't say, and then
/// so does this. A `removed` path's last part is the folder's entry, as a
/// removal takes it, so a link there is not followed.
fn judged(
    path: &Path,
    changeable: impl Fn(&Path) -> bool,
    mut link: impl FnMut(&Path) -> Option<Option<PathBuf>>,
    removed: bool,
) -> Option<PathBuf> {
    // As many links as Linux follows in one path.
    const LINKS: usize = 40;
    let mut out = PathBuf::new();
    let mut rest: Vec<_> = path
        .components()
        .rev()
        .map(|part| part.as_os_str().to_owned())
        .collect();
    let mut followed = 0;
    while let Some(part) = rest.pop() {
        // A `..` or `.` in a target is read by name, as in the path.
        if part == "." {
            continue;
        }
        if part == ".." {
            out.pop();
            continue;
        }
        let next = out.join(&part);
        let followable = followed < LINKS
            && !out.as_os_str().is_empty()
            && !(removed && rest.is_empty())
            && !changeable(&out);
        let target = match followable {
            true => link(&next)?,
            false => None,
        };
        let Some(target) = target else {
            out = next;
            continue;
        };
        followed += 1;
        // The target's parts are read on from where the link was, or
        // from the top when it names a whole path.
        if target.is_absolute() {
            out = PathBuf::from("/");
        }
        rest.extend(target.components().rev().filter_map(|part| match part {
            Component::RootDir | Component::Prefix(_) => None,
            part => Some(part.as_os_str().to_owned()),
        }));
    }
    Some(out)
}

/// `path` with `.` dropped and `..` taken back, by name alone.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// What the daemon keeps that its bots' sandboxes must leave alone or let
/// them reach.
#[derive(Debug, Clone, Default)]
pub struct Daemon {
    /// The store's path; it and the files SQLite keeps beside it stay
    /// read-only in every root.
    pub store: Option<PathBuf>,
    /// Paths read-only in every root and, when missing, not to be made,
    /// such as the files a client reads its bots' command rules from: a
    /// bot that wrote a rule could allow itself out (`--protect-path`).
    pub protected_paths: Vec<PathBuf>,
    pub socket: Option<PathBuf>,
    /// The agent binary, which a confined write runs in.
    pub helper: Option<PathBuf>,
    /// Where each sandboxed bot's own socket is opened, when the daemon
    /// listens on one.
    pub capabilities: Option<Capabilities>,
    /// Names at the top of every writable root that stay read-only and,
    /// when missing, may not be made (`--protect`), as Codex keeps its own
    /// metadata. The client starting the daemon names them.
    pub protected: Vec<String>,
    /// Names at the top of every writable root that stay read-only when
    /// they exist (`--protect-existing`).
    pub protected_existing: Vec<String>,
    /// Folders whose Unix sockets no sandboxed command reaches, the network
    /// on or not (`--wall`): where the daemon's clients keep the sockets of
    /// programs they run outside the sandbox, which do what they are asked
    /// as the person, as Codex hides its own app-server's. Each exists, by
    /// its real path.
    pub walled: Vec<PathBuf>,
    /// System folders whose files a program takes only from root or the
    /// person, which a Linux sandbox would show as nobody's: its user
    /// namespace maps the person alone, so what root owns shows as the
    /// overflow user's. Each is shown from a read-only copy the person
    /// owns (`--own-copy`). The client starting the daemon names them.
    pub owned: Vec<PathBuf>,
    /// Files of the person's a Linux sandbox shows from a read-only copy
    /// the person owns, kept in step with the file, with the text given
    /// before it ([`file_copy`]): a setting that stands for the whole file
    /// where its program takes the first value it reads
    /// (`--own-copy-file FILE=FIRST`). The client starting the daemon
    /// names them.
    pub copied_files: Vec<(PathBuf, String)>,
    /// Folders no sandboxed command reads, nor writes, links to or renames,
    /// nor anything in them, and the `read` tool doesn't read for a
    /// sandboxed bot (`--unreadable`), as Codex's `deny_read`: the person's
    /// keys, which a command that read them could send anywhere the
    /// network reaches. The client starting the daemon names them. Each by
    /// its real path once it exists; a missing one by the real path of the
    /// folder it would be made in. Folders, not files: a file replaced by
    /// a rename, as one is saved whole, would take its Linux mask with it.
    pub unreadable: Vec<PathBuf>,
    /// Where the person's daemons list themselves ([`known`]), whose
    /// stores and sockets every root and policy keeps out of reach as it
    /// does this daemon's own.
    pub known: Option<PathBuf>,
}

/// A share in a bot's own socket, which listens while any share lives,
/// as a refcounted handle or a systemd socket unit does, and a short grace
/// after the last goes. Every policy is the same whichever share it holds.
#[derive(Clone)]
pub struct Listening(pub std::sync::Arc<dyn Send + Sync>);

impl PartialEq for Listening {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}
impl Eq for Listening {}
impl std::fmt::Debug for Listening {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Listening")
    }
}

impl Daemon {
    /// Bot `bot`'s own socket, opened on first use, with a share that keeps
    /// it listening; `None` when the daemon has no socket for a sandboxed
    /// command to use.
    pub fn capability(&self, bot: i64) -> Result<Option<(PathBuf, Listening)>> {
        self.capabilities
            .as_ref()
            .map(|capabilities| (capabilities.open)(bot))
            .transpose()
    }

    /// Close bot `bot`'s own socket, once the bot is gone.
    pub fn release(&self, bot: i64) {
        if let Some(capabilities) = &self.capabilities {
            (capabilities.close)(bot);
        }
    }
}

/// The folder of the bots' own sockets, which stays read-only in every
/// root, and what opens one: the daemon's, which knows who is calling.
#[derive(Clone)]
pub struct Capabilities {
    pub folder: PathBuf,
    #[allow(clippy::type_complexity)]
    pub open: std::sync::Arc<dyn Fn(i64) -> Result<(PathBuf, Listening)> + Send + Sync>,
    pub close: std::sync::Arc<dyn Fn(i64) + Send + Sync>,
}

impl std::fmt::Debug for Capabilities {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Capabilities")
            .field("folder", &self.folder)
            .finish_non_exhaustive()
    }
}

/// The root at `path`, with what stays read-only inside it: the protected
/// folders at its top, a worktree's own git folder when it is inside the
/// root, and the daemon's store and socket.
fn root(path: &Path, daemon: &Daemon, others: &[known::Known], own: Option<&Path>) -> Result<Root> {
    // A root is kept by its real path. One reached through a link now had
    // a link swapped in since, perhaps by a bot that may write the folder
    // above it, and would carry the sandbox wherever the link points.
    if real(path).is_some_and(|real| real != path) {
        return fail_with(
            "sandbox_unavailable",
            format!(
                "{} is reached through a link, so it is not the folder the bot was given",
                path.display()
            ),
        );
    }
    // Nor one inside a walled folder: a socket there could be renamed to a
    // folder outside the wall's rule and reached by its new name.
    if let Some(wall) = daemon.walled.iter().find(|wall| path.starts_with(wall)) {
        return fail_with(
            "sandbox_unavailable",
            format!(
                "{} is inside {}, whose sockets are kept from every sandboxed command",
                path.display(),
                wall.display()
            ),
        );
    }
    let git = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".git"));
    // A git folder kept read-only whole is written nowhere, so nothing in
    // it needs holding: no placeholder is made inside it, where one would
    // be left in the repository's own folder, or inside another sandbox's
    // placeholder.
    if git && !git_folder_writable(path) {
        return Ok(Root {
            path: path.to_owned(),
            read_only: vec![path.to_owned()],
            missing: Vec::new(),
            left: Vec::new(),
            locks: Vec::new(),
            others: Vec::new(),
            opened: Vec::new(),
        });
    }
    let mut read_only = Vec::new();
    let mut opened = Vec::new();
    let mut missing = Vec::new();
    let mut left = Vec::new();
    let names = (daemon.protected.iter().map(|name| (name, true)))
        .chain(daemon.protected_existing.iter().map(|name| (name, false)));
    for (name, kept_missing) in names {
        let kept = path.join(name);
        if kept_missing && placeholder(&kept) {
            missing.push(kept);
            continue;
        }
        match std::fs::symlink_metadata(&kept) {
            Ok(found) if found.file_type().is_symlink() => return linked(&kept),
            Ok(found) => {
                // A linked worktree's .git names its repository's folder for
                // it, which stays read-only too when the root holds it.
                if name == ".git"
                    && found.is_file()
                    && let Some(gitdir) = gitdir(&kept)
                    && gitdir.starts_with(path)
                {
                    read_only.push(gitdir);
                }
                read_only.push(kept);
            }
            Err(_) if kept_missing => missing.push(kept),
            Err(_) => {}
        }
    }
    if git {
        for name in GIT_PROTECTED {
            let kept = path.join(name);
            if matches!(name, "hooks" | "modules") && placeholder(&kept) {
                missing.push(kept);
                continue;
            }
            match std::fs::symlink_metadata(&kept) {
                Ok(found) if found.file_type().is_symlink() => return linked(&kept),
                Ok(_) => read_only.push(kept),
                Err(_) if matches!(name, "hooks" | "modules") => missing.push(kept),
                Err(_) if name == "commondir" && cfg!(target_os = "linux") => left.push(kept),
                Err(_) if name == "commondir" => missing.push(kept),
                Err(_) => {}
            }
        }
        // On Linux the linked worktrees' folders stay read-only whole, but
        // for the bot's own, which it writes as it commits, its `commondir`
        // kept: a folder inside one bound read-only can't be renamed, as
        // one beside a file bound read-only can, which would take that
        // file's binding with it and leave a `commondir` of the bot's in
        // its place, read by the person's git in that worktree. One mount
        // for them all, and two for its own, however many there are.
        let worktrees = path.join("worktrees");
        match std::fs::symlink_metadata(&worktrees) {
            Ok(found) if found.file_type().is_symlink() => return linked(&worktrees),
            Ok(found) if found.is_dir() && cfg!(target_os = "linux") => {
                read_only.push(worktrees.clone());
                if let Some(own) = own.filter(|own| own.parent() == Some(worktrees.as_path())) {
                    match std::fs::symlink_metadata(own) {
                        Ok(found) if found.file_type().is_symlink() => return linked(own),
                        Ok(found) if found.is_dir() => {
                            let kept = own.join("commondir");
                            match std::fs::symlink_metadata(&kept) {
                                Ok(found) if found.file_type().is_symlink() => {
                                    return linked(&kept);
                                }
                                Ok(_) => {
                                    opened.push(own.to_owned());
                                    read_only.push(kept);
                                }
                                // A worktree's folder names its repository;
                                // one that doesn't stays read-only.
                                Err(_) => {}
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        // Elsewhere each linked worktree's folder names the repository's
        // folder for it the same way, its folder anchored by Seatbelt. One
        // without it was made since, and serves only a checkout the bot made
        // to name it. Nothing this deep is removed after a command, as the
        // root's own `commondir` is: its folder could be swapped for a link
        // elsewhere before the removal follows.
        for entry in std::fs::read_dir(&worktrees)
            .into_iter()
            .filter(|_| !cfg!(target_os = "linux"))
            .flatten()
            .flatten()
        {
            match entry.file_type() {
                Ok(found) if found.is_symlink() => return linked(&entry.path()),
                Ok(found) if found.is_dir() => {}
                _ => continue,
            }
            let kept = entry.path().join("commondir");
            match std::fs::symlink_metadata(&kept) {
                Ok(found) if found.file_type().is_symlink() => return linked(&kept),
                Ok(_) => read_only.push(kept),
                Err(_) => {}
            }
        }
    }
    if let Some(store) = &daemon.store
        && store.starts_with(path)
    {
        hold_journals(store);
        read_only.extend(store_files(store).filter(|file| file.exists()));
    }
    // A folder it may not read is masked as each command starts, made
    // first if it is gone ([`hold`]), so none is ever open to a command.
    for kept in (daemon.unreadable.iter()).filter(|kept| kept.starts_with(path)) {
        if std::fs::symlink_metadata(kept).is_ok_and(|found| found.file_type().is_symlink()) {
            return linked(kept);
        }
    }
    // A walled folder too: renamed, or a socket made in it, it would let
    // one out past its rule.
    for kept in (daemon.protected_paths.iter())
        .chain(&daemon.walled)
        .chain(locks())
        .chain(copies())
        .filter(|kept| kept.starts_with(path))
    {
        match std::fs::symlink_metadata(kept) {
            Ok(found) if found.file_type().is_symlink() => return linked(kept),
            Ok(_) => read_only.push(kept.clone()),
            Err(_) => missing.push(kept.clone()),
        }
    }
    // A socket the bot could rename or link would be reachable by its new
    // name, past a rule that names the old one; and one bot could put its
    // own listener in place of another's.
    for kept in daemon
        .socket
        .iter()
        .chain(daemon.capabilities.as_ref().map(|c| &c.folder))
    {
        if kept.starts_with(path) && kept.exists() {
            read_only.push(kept.clone());
        }
    }
    // Every other daemon's store and socket, as this one's: written, the
    // store would change what that daemon runs unsandboxed; renamed or
    // linked, the socket would be reached by another name.
    let others = (others.iter())
        .inspect(|other| {
            if other.store.starts_with(path) {
                hold_journals(&other.store);
            }
        })
        .flat_map(|other| {
            let sockets =
                (other.socket.iter()).flat_map(|socket| [socket.clone(), bots_folder(socket)]);
            store_files(&other.store).chain(sockets).collect::<Vec<_>>()
        })
        .filter(|kept| kept.starts_with(path) && kept != path)
        .collect();
    let locks = match cfg!(target_os = "linux") {
        true => missing.iter().filter_map(|kept| made_lock(kept)).collect(),
        false => Vec::new(),
    };
    Ok(Root {
        path: path.to_owned(),
        read_only,
        missing,
        left,
        locks,
        others,
        opened,
    })
}

/// Whether a git folder may be written with its hooks and config kept: not
/// when its config is missing, which git would read once made, nor when it
/// turns on per-worktree config or includes other files, which git reads
/// wherever they are, made or not. Any mention of either counts.
fn git_folder_writable(path: &Path) -> bool {
    std::fs::read_to_string(path.join("config")).is_ok_and(|config| {
        let config = config.to_ascii_lowercase();
        !config.contains("worktreeconfig") && !config.contains("include")
    })
}

/// A symlink the sandbox may replace can't be held read-only: whatever is
/// mounted over its target, the link itself can be swapped for a folder of
/// the command's own. Codex refuses it too.
fn linked(kept: &Path) -> Result<Root> {
    fail_with(
        "sandbox_unavailable",
        format!(
            "{} is a symlink in a folder the bot may write, so it can't be kept read-only",
            kept.display()
        ),
    )
}

/// Where the daemon binds its per-bot sockets. Every sandbox derives this
/// same path for the daemons it protects, even before the folder exists.
pub fn bots_folder(socket: &Path) -> PathBuf {
    use std::os::unix::net::SocketAddr;
    let mut name = socket.as_os_str().to_owned();
    name.push(".d");
    let beside = PathBuf::from(name);
    if SocketAddr::from_pathname(beside.join("9223372036854775807")).is_ok() {
        return beside;
    }
    // The canonical socket path identifies this daemon, including one
    // started with an explicit socket. prepare checks ownership and mode.
    let digest = aws_lc_rs::digest::digest(
        &aws_lc_rs::digest::SHA256,
        socket.as_os_str().as_encoded_bytes(),
    );
    let id = u128::from_be_bytes(digest.as_ref()[..16].try_into().unwrap());
    let tmp = if cfg!(target_os = "macos") {
        "/private/tmp"
    } else {
        "/tmp"
    };
    Path::new(tmp).join(format!("agent-bots-{id:032x}"))
}

/// The store and the files SQLite and the daemon keep beside it.
fn store_files(store: &Path) -> impl Iterator<Item = PathBuf> + '_ {
    ["", "-wal", "-shm", "-journal", ".owner-lock", ".sock"]
        .into_iter()
        .map(move |suffix| {
            let mut file = store.as_os_str().to_owned();
            file.push(suffix);
            PathBuf::from(file)
        })
}

/// SQLite's journals beside `store`, made empty where they are not there,
/// on Linux, where a read-only mount needs something to mount: a missing
/// one the command could make, a write-ahead log or a hot journal that
/// SQLite would play into the store as it next opens it. Empty, each is
/// what SQLite takes a missing one for. Made only beside a store that is
/// there, and never over anything, a link included.
fn hold_journals(store: &Path) {
    use std::os::unix::fs::OpenOptionsExt;
    if !cfg!(target_os = "linux") || !std::fs::metadata(store).is_ok_and(|meta| meta.is_file()) {
        return;
    }
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut file = store.as_os_str().to_owned();
        file.push(suffix);
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(PathBuf::from(file));
    }
}

/// The folder a `.git` file names, by its real path.
fn gitdir(file: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(file).ok()?;
    let named = text.trim().strip_prefix("gitdir:")?.trim();
    std::fs::canonicalize(file.parent()?.join(named)).ok()
}

fn depth(path: &Path) -> usize {
    path.components().count()
}

/// Whether this machine can run sandboxed commands, or why not: refused when
/// a bot asks for a sandbox, rather than running it unconfined.
pub fn available() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        linux::checked().map(|_| ())
    }
    #[cfg(target_os = "macos")]
    {
        static FOUND: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        match FOUND.get_or_init(|| Path::new(macos::SANDBOX_EXEC).exists()) {
            true => Ok(()),
            false => fail_with(
                "sandbox_unavailable",
                format!("{} is missing", macos::SANDBOX_EXEC),
            ),
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        fail_with(
            "sandbox_unavailable",
            "sandboxes run on Linux, with bubblewrap, and on macOS",
        )
    }
}

/// `program args...` confined by `policy`, and what tells when its
/// sandbox is gone, for its [`Hold`] to wait for. A detached command
/// outlives the daemon, so its sandbox doesn't die with it.
pub fn command(
    policy: &Policy,
    program: &str,
    args: &[&str],
    detached: bool,
) -> Result<(tokio::process::Command, Ended)> {
    #[cfg(target_os = "linux")]
    {
        linux::command(policy, program, args, detached)
    }
    #[cfg(target_os = "macos")]
    {
        let _ = detached;
        Ok((macos::command(policy, program, args)?, Ended(None)))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (policy, program, args, detached);
        available().map(|()| unreachable!())
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{Policy, bots_folder, depth};
    use crate::{Error, Result};
    use std::path::{Path, PathBuf};

    /// Where a system's bubblewrap is installed. Never looked up on PATH:
    /// a PATH entry inside a folder a bot may write, such as an activated
    /// virtualenv, would let a sandboxed command plant the next one's
    /// `bwrap`, which runs before any sandbox exists.
    const BWRAP: [&str; 4] = [
        "/usr/bin/bwrap",
        "/bin/bwrap",
        "/usr/local/bin/bwrap",
        "/run/current-system/sw/bin/bwrap",
    ];

    /// The bubblewrap a package build depends on, fixed when the daemon is
    /// built (`AGENT_BWRAP`), as Flatpak's `--with-system-bubblewrap`:
    /// Homebrew on Linux installs it under its own prefix.
    const BUILT_WITH: Option<&str> = option_env!("AGENT_BWRAP");

    fn places() -> impl Iterator<Item = &'static str> {
        BUILT_WITH.into_iter().chain(BWRAP)
    }

    fn bwrap() -> Option<PathBuf> {
        places().map(PathBuf::from).find(|found| found.is_file())
    }

    /// The bubblewrap that sandboxes run in, checked once: a daemon
    /// doesn't look again for each command.
    pub(super) fn checked() -> Result<&'static PathBuf> {
        static CHECKED: std::sync::OnceLock<std::result::Result<PathBuf, String>> =
            std::sync::OnceLock::new();
        CHECKED
            .get_or_init(check)
            .as_ref()
            .map_err(|detail| Error::with("sandbox_unavailable", detail.clone()))
    }

    /// Whether bubblewrap is here and may make the namespaces a sandbox
    /// needs, or what to do about it.
    fn check() -> std::result::Result<PathBuf, String> {
        let Some(bwrap) = bwrap() else {
            return Err(format!(
                "bubblewrap (bwrap) is not in {}; install it, for example with \
                     `apt install bubblewrap` or `dnf install bubblewrap`",
                places().collect::<Vec<_>>().join(", ")
            ));
        };
        let output = std::process::Command::new(&bwrap)
            .args([
                "--ro-bind",
                "/",
                "/",
                "--dev",
                "/dev",
                "--unshare-user",
                "--unshare-pid",
                "--unshare-net",
                "--proc",
                "/proc",
                "--",
                "true",
            ])
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| format!("{} could not run: {e}", bwrap.display()))?;
        match output.status.success() {
            true => Ok(bwrap),
            false => Err(format!(
                "{} could not make a sandbox ({}); user namespaces may be off, as \
                 kernel.unprivileged_userns_clone=0 or an AppArmor rule makes them",
                bwrap.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            )),
        }
    }

    /// bubblewrap's arguments for `policy`, as Codex builds them: the whole
    /// filesystem read-only, each root bound writable from the shallowest
    /// down with its read-only folders bound back over it, and what may not
    /// be made held by an empty read-only folder in its place.
    pub(super) fn args(policy: &Policy, detached: bool) -> Vec<String> {
        let path = |path: &Path| path.to_string_lossy().into_owned();
        // A new session keeps the command from typing into the daemon's
        // terminal (TIOCSTI).
        let mut args: Vec<String> = vec!["--new-session".into()];
        if !detached {
            args.push("--die-with-parent".into());
        }
        args.extend(["--ro-bind", "/", "/", "--dev", "/dev"].map(String::from));
        for root in &policy.roots {
            args.extend(["--bind".into(), path(&root.path), path(&root.path)]);
            // Each folder between the root and a folder it may not read, or
            // one it keeps, bound on itself so no command renames it: moved,
            // it would take the folder out from under its mask or binding
            // and its name, as Seatbelt anchors them. None inside what is
            // bound already, which can't be renamed. First, under what is
            // kept read-only.
            let bound = |folder: &Path| {
                (root.read_only.iter())
                    .chain(&root.opened)
                    .any(|kept| folder.starts_with(kept))
            };
            let mut anchored: Vec<&Path> = (policy.unreadable.iter())
                .chain(&root.read_only)
                .chain(&root.missing)
                .chain(&root.others)
                .filter(|hidden| hidden.starts_with(&root.path))
                .flat_map(|hidden| hidden.ancestors().skip(1))
                .filter(|folder| {
                    folder.starts_with(&root.path) && *folder != root.path && !bound(folder)
                })
                .collect();
            anchored.sort();
            anchored.dedup();
            anchored.sort_by_key(|folder| depth(folder));
            for folder in anchored {
                args.extend(["--bind".into(), path(folder), path(folder)]);
            }
            // Shallowest first, so a folder opened again inside one kept
            // read-only is bound over it, and what it keeps over that.
            let mut kept: Vec<(&PathBuf, &str)> = (root.read_only.iter())
                .map(|kept| (kept, "--ro-bind"))
                .chain(root.opened.iter().map(|opened| (opened, "--bind")))
                .collect();
            kept.sort_by_key(|(path, _)| depth(path));
            for (kept, how) in kept {
                args.extend([how.into(), path(kept), path(kept)]);
            }
            // Another daemon's, while they are there: one gone since is
            // skipped rather than stopping the command.
            for kept in &root.others {
                args.extend(["--ro-bind-try".into(), path(kept), path(kept)]);
            }
            for missing in &root.missing {
                args.extend([
                    "--perms".into(),
                    "555".into(),
                    "--tmpfs".into(),
                    path(missing),
                    "--remount-ro".into(),
                    path(missing),
                ]);
            }
        }
        // In the daemon's socket's place, the bot's own, which answers as
        // that bot and only as far as a sandboxed bot may go; without one,
        // nothing.
        if let Some(socket) = &policy.socket {
            match &policy.capability {
                Some(own) => {
                    args.extend(["--bind".into(), path(own), path(socket)]);
                    // The other bots' sockets are out of sight: each would
                    // answer as its own bot.
                    if let Some(folder) = own.parent() {
                        args.extend([
                            "--perms".into(),
                            "555".into(),
                            "--tmpfs".into(),
                            path(folder),
                            "--remount-ro".into(),
                            path(folder),
                        ]);
                    }
                }
                None => args.extend(["--ro-bind".into(), "/dev/null".into(), path(socket)]),
            }
        }
        // Nor any other daemon's socket, nor its bots' own sockets: each
        // that is there now, as one that stopped took its socket with it.
        for socket in &policy.daemons {
            if std::fs::symlink_metadata(socket).is_ok() {
                args.extend(["--ro-bind".into(), "/dev/null".into(), path(socket)]);
            }
            let folder = bots_folder(socket);
            if folder.is_dir() {
                args.extend([
                    "--perms".into(),
                    "555".into(),
                    "--tmpfs".into(),
                    path(&folder),
                    "--remount-ro".into(),
                    path(&folder),
                ]);
            }
        }
        // A walled folder is out of sight, as Codex hides its app-server's
        // sockets: a socket in it is a program's that acts as the person.
        // Always mounted over, so no command runs without it; one removed
        // since the daemon made it is made again first ([`hold`]), and one
        // that could not be fails the command, closed.
        for folder in &policy.walled {
            args.extend([
                "--perms".into(),
                "555".into(),
                "--tmpfs".into(),
                path(folder),
                "--remount-ro".into(),
                path(folder),
            ]);
        }
        // The person's copies of the system's folders whose files a
        // program checks the owner of, over the folders themselves.
        for (copy, place) in &policy.owned {
            args.extend(["--ro-bind".into(), path(copy), path(place)]);
        }
        // A folder it may not read, out of sight, as Codex hides its
        // deny_read folders: empty and closed, over every other mount, so
        // nothing shown later opens it again. [`super::hold`] made it if it
        // was gone. What changes in it, the keys file replaced by a rename
        // among them, stays behind the mask, which a rename over a file
        // would take down.
        // One in a walled folder is out of sight with it already, the
        // walled folder an empty one with no place for its mask.
        for hidden in (policy.unreadable.iter()).filter(|hidden| {
            !policy
                .walled
                .iter()
                .any(|walled| hidden.starts_with(walled))
        }) {
            args.extend([
                "--perms".into(),
                "000".into(),
                "--tmpfs".into(),
                path(hidden),
                "--remount-ro".into(),
                path(hidden),
            ]);
        }
        args.extend(["--unshare-user", "--unshare-pid", "--unshare-ipc"].map(String::from));
        if !policy.network {
            args.push("--unshare-net".into());
        }
        args.extend(["--proc", "/proc", "--cap-drop", "ALL"].map(String::from));
        args
    }

    pub(super) fn command(
        policy: &Policy,
        program: &str,
        rest: &[&str],
        detached: bool,
    ) -> Result<(tokio::process::Command, super::Ended)> {
        use std::os::fd::AsRawFd;
        let mut command = tokio::process::Command::new(checked()?);
        let filter = loaded(&filter(policy)?)?;
        let (ended, sync) = pipe()?;
        command
            .args(args(policy, detached))
            .args(super::lock_args(policy))
            .arg("--seccomp")
            .arg(filter.as_raw_fd().to_string())
            .arg("--sync-fd")
            .arg(sync.as_raw_fd().to_string());
        let mut kept = vec![filter, sync];
        if let Some(own) = &policy.capability {
            let own = own.clone();
            let connect = move || {
                let own = own.clone();
                async move {
                    // Handed blocking, as a connection made in the command
                    // would be: the description is shared, and its flags
                    // with it.
                    (tokio::net::UnixStream::connect(&own).await)
                        .and_then(tokio::net::UnixStream::into_std)
                        .and_then(|stream| stream.set_nonblocking(false).map(|()| stream))
                        .ok()
                }
            };
            if let Some(handing) = handing(connect)? {
                command.env(
                    agent_client::socket::HANDED,
                    handing.as_raw_fd().to_string(),
                );
                kept.push(handing);
            }
        }
        command.arg("--");
        if !policy.domains.is_empty() {
            // Its own network is off: the bridge, the agent binary, is the
            // way out, to the proxy alone.
            let helper = (policy.helper.as_deref()).ok_or_else(|| {
                Error::with(
                    "sandbox_unavailable",
                    "domains need the agent binary, to bridge commands to the proxy",
                )
            })?;
            let domains = std::sync::Arc::new(policy.domains.clone());
            let proxied = move || {
                let domains = domains.clone();
                async move {
                    let (ours, theirs) = std::os::unix::net::UnixStream::pair().ok()?;
                    ours.set_nonblocking(true).ok()?;
                    let ours = tokio::net::UnixStream::from_std(ours).ok()?;
                    tokio::spawn(super::proxy::serve(ours, domains));
                    Some(theirs)
                }
            };
            let handing = handing(proxied)?.ok_or_else(|| {
                Error::with(
                    "sandbox_unavailable",
                    "domains need the daemon's runtime, which answers the proxy",
                )
            })?;
            command.env(super::PROXY_FD, handing.as_raw_fd().to_string());
            kept.push(handing);
            command.arg(helper).arg(super::PROXY).arg("--");
        }
        command.arg(program).args(rest);
        // SAFETY: fcntl is async-signal-safe; it only lets the descriptors
        // through exec, into bubblewrap and the command.
        unsafe {
            command.pre_exec(move || {
                for fd in &kept {
                    if libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        Ok((command, super::Ended(Some(ended))))
    }

    /// A pipe, both ends closed on exec: its read end and its write end.
    fn pipe() -> Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
        use std::os::fd::FromRawFd;
        let mut ends = [0; 2];
        // SAFETY: pipe2 writes two descriptors into the array it is given.
        if unsafe { libc::pipe2(ends.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: both are open, and each is owned only here.
        Ok(unsafe {
            (
                std::os::fd::OwnedFd::from_raw_fd(ends[0]),
                std::os::fd::OwnedFd::from_raw_fd(ends[1]),
            )
        })
    }

    /// The seccomp filter bubblewrap loads, as Codex's Linux sandbox and
    /// Claude Code's build theirs: what no path rule can hold. With the
    /// network off a command makes no Unix socket, as Codex's reaches
    /// none, since bubblewrap's read-only root still lets one connect to
    /// any it sees, Docker's or a desktop session's bus that runs anything
    /// asked; one that names some is let make them all, since seccomp
    /// can't tell them apart by path, as Claude Code's
    /// `allowAllUnixSockets`. With the network on it makes any, as Codex's
    /// full network does; the daemon's and the other bots' stay out of
    /// sight. A pair (socketpair) it still makes, which reaches nothing.
    /// io_uring, which makes sockets past seccomp, it never sets up. Any
    /// other architecture's calls, which number them otherwise, end the
    /// command.
    pub(super) fn filter(policy: &Policy) -> Result<Vec<u8>> {
        use seccompiler::{
            BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition,
            SeccompFilter, SeccompRule, TargetArch,
        };
        let invalid = |error: &dyn std::fmt::Display| {
            Error::with("sandbox_unavailable", format!("seccomp filter: {error}"))
        };
        let arch = TargetArch::try_from(std::env::consts::ARCH).map_err(|e| invalid(&e))?;
        let unix = SeccompRule::new(vec![
            SeccompCondition::new(
                0,
                SeccompCmpArgLen::Dword,
                SeccompCmpOp::Eq,
                libc::AF_UNIX as u64,
            )
            .map_err(|e| invalid(&e))?,
        ])
        .map_err(|e| invalid(&e))?;
        // A pair of datagram sockets, which Linux makes of a raw one too,
        // sends to any socket's path, not only to its other end.
        let datagrams = [libc::SOCK_DGRAM, libc::SOCK_RAW].map(|kind| {
            SeccompRule::new(vec![
                SeccompCondition::new(
                    0,
                    SeccompCmpArgLen::Dword,
                    SeccompCmpOp::Eq,
                    libc::AF_UNIX as u64,
                )?,
                SeccompCondition::new(
                    1,
                    SeccompCmpArgLen::Dword,
                    SeccompCmpOp::MaskedEq(0xf),
                    kind as u64,
                )?,
            ])
        });
        let datagrams = (datagrams.into_iter())
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| invalid(&e))?;
        let mut rules = std::collections::BTreeMap::new();
        rules.insert(libc::SYS_io_uring_setup, Vec::new());
        let blocked = !policy.network && policy.sockets.is_empty();
        if blocked {
            rules.insert(libc::SYS_socket, vec![unix.clone()]);
            rules.insert(libc::SYS_socketpair, datagrams.clone());
        }
        // x86_64's x32 calls share its architecture, numbered past this bit.
        if arch == TargetArch::x86_64 {
            const X32: i64 = 0x4000_0000;
            rules.insert(X32 | libc::SYS_io_uring_setup, Vec::new());
            if blocked {
                rules.insert(X32 | libc::SYS_socket, vec![unix]);
                rules.insert(X32 | libc::SYS_socketpair, datagrams);
            }
        }
        let filter = SeccompFilter::new(
            rules,
            SeccompAction::Allow,
            SeccompAction::Errno(libc::EPERM as u32),
            arch,
        )
        .map_err(|e| invalid(&e))?;
        let program: BpfProgram = filter.try_into().map_err(|e| invalid(&e))?;
        Ok((program.iter())
            .flat_map(|instruction| {
                let mut bytes = Vec::with_capacity(8);
                bytes.extend(instruction.code.to_ne_bytes());
                bytes.push(instruction.jt);
                bytes.push(instruction.jf);
                bytes.extend(instruction.k.to_ne_bytes());
                bytes
            })
            .collect())
    }

    /// `program` behind a pipe's read end, for bubblewrap's `--seccomp`.
    fn loaded(program: &[u8]) -> Result<std::os::fd::OwnedFd> {
        use std::io::Write;
        let (read, write) = std::io::pipe()?;
        // A filter is far smaller than a pipe holds, so this never waits.
        std::fs::File::from(std::os::fd::OwnedFd::from(write)).write_all(program)?;
        Ok(read.into())
    }

    /// The descriptor a command asks the daemon for connections on, as
    /// [`agent_client::socket::handed`] does: each request is answered
    /// with the connection `answer` opens, to the bot's own socket or to
    /// the proxy, for as long as any process of the command holds it.
    /// Without a runtime to answer on, none.
    fn handing<F, Answer>(answer: F) -> Result<Option<std::os::fd::OwnedFd>>
    where
        F: Fn() -> Answer + Send + 'static,
        Answer: std::future::Future<Output = Option<std::os::unix::net::UnixStream>> + Send,
    {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return Ok(None);
        };
        let mut pair = [0; 2];
        // SAFETY: socketpair writes two descriptors into `pair`.
        if unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                0,
                pair.as_mut_ptr(),
            )
        } < 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: both are new and owned here alone.
        let (ours, theirs) =
            unsafe { (OwnedFd::from_raw_fd(pair[0]), OwnedFd::from_raw_fd(pair[1])) };
        // The command's end blocks; each asker waits on it as it chooses.
        // SAFETY: fcntl on a descriptor owned here.
        unsafe {
            let flags = libc::fcntl(theirs.as_raw_fd(), libc::F_GETFL);
            libc::fcntl(theirs.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK);
        }
        // SAFETY: the OwnedFd is open, and it keeps the one descriptor
        // until the AsyncFd that owns it drops.
        let ours =
            unsafe { tokio::io::unix::AsyncFd::register(ours) }.map_err(std::io::Error::from)?;
        runtime.spawn(async move {
            let mut byte = [0u8; 1];
            loop {
                let Ok(mut ready) = ours.readable().await else {
                    return;
                };
                // SAFETY: the byte outlives the call.
                let read = unsafe {
                    libc::recv(
                        ours.get_ref().as_raw_fd(),
                        byte.as_mut_ptr().cast(),
                        1,
                        libc::MSG_DONTWAIT,
                    )
                };
                match read {
                    // Every process of the command has let it go.
                    0 => return,
                    n if n < 0 => {
                        match std::io::Error::last_os_error().kind() {
                            std::io::ErrorKind::WouldBlock => ready.clear_ready(),
                            std::io::ErrorKind::Interrupted => {}
                            _ => return,
                        }
                        continue;
                    }
                    _ => {}
                }
                let stream = answer().await;
                let fd = ours.get_ref().as_raw_fd();
                let _ = agent_client::socket::hand(fd, stream.as_ref().map(|s| s.as_raw_fd()));
            }
        });
        Ok(Some(theirs))
    }
}

/// The folders of `places` ([`Daemon::owned`]) that are here, each with
/// the person's copy, made again when a file in the folder changes; none
/// on another system than Linux, or for root, whom the namespace maps as
/// itself. Off the runtime thread, as a policy is built: a folder
/// unchanged since costs a listing and a look at each file. One that can't
/// be copied is left as it shows.
fn owned(places: &[PathBuf]) -> Vec<(PathBuf, PathBuf)> {
    // SAFETY: geteuid takes nothing and can't fail.
    if places.is_empty() || !cfg!(target_os = "linux") || unsafe { libc::geteuid() } == 0 {
        return Vec::new();
    }
    let Some(copies) = copies() else {
        return Vec::new();
    };
    (places.iter())
        .filter_map(|place| {
            let copy = copies.join(place.strip_prefix("/").ok()?);
            copied(place, &copy).then(|| (copy, place.clone()))
        })
        .collect()
}

/// Where the copies of [`Daemon::owned`]'s folders are kept, `~/.agent/copies`,
/// each under its own path, read-only to every sandbox; `None` when it
/// can't be made, and on another system than Linux, which needs none.
fn copies() -> Option<&'static PathBuf> {
    static COPIES: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    COPIES
        .get_or_init(|| {
            use std::os::unix::fs::DirBuilderExt;
            if !cfg!(target_os = "linux") {
                return None;
            }
            let folder = agent_client::config::home()?.join("copies");
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&folder)
                .ok()?;
            std::fs::canonicalize(&folder).ok()
        })
        .as_ref()
}

/// What a folder's files are, to tell when one changed: each regular file's
/// name, size, change time and inode, as the folder lists them, following
/// links; `None` when the folder isn't there.
type Stamp = Vec<(std::ffi::OsString, u64, i64, i64, u64)>;

fn stamp(place: &Path) -> Option<Stamp> {
    use std::os::unix::fs::MetadataExt;
    let mut files: Stamp = (std::fs::read_dir(place).ok()?.flatten())
        .filter_map(|entry| {
            let found = std::fs::metadata(entry.path()).ok()?;
            found.is_file().then(|| {
                let name = entry.file_name();
                (
                    name,
                    found.len(),
                    found.ctime(),
                    found.ctime_nsec(),
                    found.ino(),
                )
            })
        })
        .collect();
    files.sort();
    Some(files)
}

/// Whether `copy` holds `place`'s files now, as the person's own, copying
/// them when they changed since this daemon last did. Each file is written
/// beside its name and renamed over it, so a sandbox running reads the old
/// file or the new one, never part of either; the folder stays, as
/// bubblewrap binds it by name.
fn copied(place: &Path, copy: &Path) -> bool {
    static STAMPS: std::sync::Mutex<Vec<(PathBuf, Stamp)>> = std::sync::Mutex::new(Vec::new());
    let Some(now) = stamp(place) else {
        return false;
    };
    let mut stamps = STAMPS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let kept = stamps.iter().position(|(kept, _)| kept == place);
    let fresh = kept.is_some_and(|at| stamps[at].1 == now)
        && std::fs::symlink_metadata(copy).is_ok_and(|found| found.is_dir());
    if fresh {
        return true;
    }
    if locked(copy, || copy_files(place, copy, &now)).is_err() {
        return false;
    }
    match kept {
        Some(at) => stamps[at].1 = now,
        None => stamps.push((place.to_owned(), now)),
    }
    true
}

/// `write` run while this process alone writes `copy`: another daemon or
/// an `agent sandbox` of the person's copies into the same folder, and
/// would take a file half written for its own, or remove it. The lock is
/// a file beside the folder, which no sandbox sees, held until `write`
/// returns.
fn locked(copy: &Path, write: impl FnOnce() -> std::io::Result<()>) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let above = copy.parent().ok_or(std::io::ErrorKind::InvalidInput)?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(above)?;
    let mut name = copy.as_os_str().to_owned();
    name.push(".lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(name)?;
    lock.lock()?;
    write()
}

/// `files` of `place` into `copy`, owner-only, and nothing else in it.
fn copy_files(place: &Path, copy: &Path, files: &Stamp) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(copy)?;
    // Never through a link: bubblewrap would bind wherever it points.
    if !std::fs::symlink_metadata(copy)?.is_dir() {
        return Err(std::io::ErrorKind::AlreadyExists.into());
    }
    for (name, ..) in files {
        // One it can't read, it couldn't read outside a sandbox either.
        let Ok(content) = std::fs::read(place.join(name)) else {
            continue;
        };
        let mut part = std::ffi::OsString::from(".");
        part.push(name);
        part.push(".part");
        let part = copy.join(part);
        let _ = std::fs::remove_file(&part);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&part)?;
        file.write_all(&content)?;
        std::fs::rename(&part, copy.join(name))?;
    }
    for entry in std::fs::read_dir(copy)?.flatten() {
        let name = entry.file_name();
        let wanted = files.iter().any(|(file, ..)| *file == name)
            && entry.file_type().is_ok_and(|kind| kind.is_file());
        if !wanted {
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => std::fs::remove_dir_all(entry.path())?,
                _ => std::fs::remove_file(entry.path())?,
            }
        }
    }
    Ok(())
}

/// A file of the person's, `place`, as every Linux sandbox shows it
/// ([`Daemon::copied_files`]), a link followed (bubblewrap binds over no
/// link): `(copy, file)`, a read-only copy the person owns that says
/// `first` before the file, kept in step with it, bound over the file.
/// `None` when there is none and on another system than Linux, whose
/// sandbox binds nothing. One kept unreadable is not shown, and a copy
/// made before it was is removed: under `~/.agent`, which sandboxes read,
/// it would show what the folder now hides.
fn file_copy(place: &Path, first: &str, unreadable: &[PathBuf]) -> Option<(PathBuf, PathBuf)> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    copied_in(copies()?, place, first, unreadable)
}

/// [`file_copy`], its copy kept under `copies`.
fn copied_in(
    copies: &Path,
    place: &Path,
    first: &str,
    unreadable: &[PathBuf],
) -> Option<(PathBuf, PathBuf)> {
    // Named by the text too: another daemon that puts other text first
    // keeps its own copy rather than writing over this one's.
    let copy_of = |file: &Path| {
        let mut named = copies.join(file.strip_prefix("/").ok()?).into_os_string();
        named.push(format!(".{:016x}", fnv(first.as_bytes())));
        Some(PathBuf::from(named))
    };
    let file = std::fs::canonicalize(place).ok();
    let hidden = |path: &Path| unreadable.iter().any(|hidden| path.starts_with(hidden));
    if hidden(place) || file.as_deref().is_some_and(hidden) {
        for kept in [Some(place), file.as_deref()].into_iter().flatten() {
            if let Some(copy) = copy_of(kept) {
                let _ = locked(&copy, || match std::fs::remove_file(&copy) {
                    Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
                    _ => Ok(()),
                });
            }
        }
        return None;
    }
    let file = file?;
    let copy = copy_of(&file)?;
    copied_file(&file, first, &copy).then_some((copy, file))
}

/// FNV-1a over `bytes`: the same in every build, as a name kept on disk
/// between daemons must be.
fn fnv(bytes: &[u8]) -> u64 {
    (bytes.iter()).fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Whether `copy` holds `file` now, `first` before it, writing it again
/// when the file changed since this daemon last did, or the copy is gone.
/// Written beside its name and renamed over it, as [`copied`] writes a
/// folder's files.
fn copied_file(file: &Path, first: &str, copy: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    type FileStamp = (u64, i64, i64, u64);
    static STAMPS: std::sync::Mutex<Vec<(PathBuf, FileStamp, String)>> =
        std::sync::Mutex::new(Vec::new());
    let Ok(found) = std::fs::metadata(file) else {
        return false;
    };
    if !found.is_file() {
        return false;
    }
    let now = (found.len(), found.ctime(), found.ctime_nsec(), found.ino());
    let mut stamps = STAMPS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let kept = stamps.iter().position(|(kept, ..)| kept == file);
    let fresh = kept.is_some_and(|at| stamps[at].1 == now && stamps[at].2 == first)
        && std::fs::symlink_metadata(copy).is_ok_and(|found| found.is_file());
    if fresh {
        return true;
    }
    if locked(copy, || copy_file(file, first, copy)).is_err() {
        return false;
    }
    match kept {
        Some(at) => stamps[at] = (file.to_owned(), now, first.to_owned()),
        None => stamps.push((file.to_owned(), now, first.to_owned())),
    }
    true
}

/// `file` into `copy`, owner-only, `first` before it.
fn copy_file(file: &Path, first: &str, copy: &Path) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let content = std::fs::read(file)?;
    let (Some(folder), Some(name)) = (copy.parent(), copy.file_name()) else {
        return Err(std::io::ErrorKind::InvalidInput.into());
    };
    let mut part = std::ffi::OsString::from(".");
    part.push(name);
    part.push(".part");
    let part = folder.join(part);
    let _ = std::fs::remove_file(&part);
    let mut written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&part)?;
    written.write_all(first.as_bytes())?;
    written.write_all(&content)?;
    // Never over a link or a folder: bubblewrap would bind wherever it
    // points, or refuse every command binding a folder over a file.
    if std::fs::symlink_metadata(copy).is_ok_and(|found| !found.is_file()) {
        let _ = std::fs::remove_file(&part);
        return Err(std::io::ErrorKind::AlreadyExists.into());
    }
    std::fs::rename(&part, copy)
}

/// Whether `path`, a name kept from being made, holds only a placeholder:
/// an empty folder, which a sandbox's read-only mount made or still makes
/// there. It is taken as missing, as Codex takes it: mounted over afresh,
/// never bound from what is on disk, which could be gone by the time
/// bubblewrap binds it.
fn placeholder(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|found| found.is_dir())
        && std::fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_none())
}

/// Where each placeholder's lock file is kept, `~/.agent/placeholders`,
/// by its real path, read-only to every sandbox; `None` when it can't be
/// made, and then no placeholder is ever removed.
///
/// bubblewrap makes a placeholder on the host for each read-only mount
/// over a missing name; removing it while any sandbox still mounts it
/// lifts that mount there, so the command could make what it kept. Each
/// sandbox holds a read lock on the placeholder's lock file while it runs,
/// bubblewrap's own init holding it (`--lock-file`), out of the command's
/// reach, and the daemon or CLI that started it holding another from
/// before the sandbox is made until it is gone, init with it ([`Ended`]).
/// The last one out lets its own read lock go, takes the write lock and
/// removes the placeholder, when it is one this sandbox made: an empty
/// folder, the one recorded in the lock file. The kernel
/// lets the locks go with whoever held them, so a crash leaves none held,
/// and a placeholder left by one is removed by the next sandbox that
/// holds it. One made by anything else, Codex included, is left, as Codex
/// leaves what it did not make.
fn locks() -> Option<&'static PathBuf> {
    static LOCKS: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    LOCKS
        .get_or_init(|| {
            use std::os::unix::fs::DirBuilderExt;
            let folder = agent_client::config::home()?.join("placeholders");
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&folder)
                .ok()?;
            std::fs::canonicalize(&folder).ok()
        })
        .as_ref()
}

/// The lock file of the placeholder at `path`: named by a stable hash of
/// the path, so every process and release finds the same one. Two paths
/// sharing one only wait for each other's sandboxes.
fn lock_file(path: &Path) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    // FNV-1a.
    let hash = (path.as_os_str().as_bytes().iter())
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
        });
    Some(locks()?.join(format!("{hash:016x}")))
}

/// The lock file of the placeholder at `path`, made if it isn't there;
/// `None` when it can't be.
fn made_lock(path: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::OpenOptionsExt;
    let lock = lock_file(path)?;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&lock)
        .ok()?;
    Some(lock)
}

/// The arguments that have bubblewrap's init hold `policy`'s placeholders'
/// locks while the sandbox runs: those made as its roots were read, so
/// building a command touches no file.
#[cfg(target_os = "linux")]
fn lock_args(policy: &Policy) -> Vec<String> {
    (policy.roots.iter())
        .flat_map(|root| &root.locks)
        .flat_map(|lock| ["--lock-file".into(), lock.to_string_lossy().into_owned()])
        .collect()
}

/// A lock on the whole of `file`, open file description's own, so each
/// holder's counts apart even in one process; `wait` blocks for it.
fn lock(file: &std::fs::File, kind: libc::c_int, wait: bool) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    #[cfg(target_os = "linux")]
    let command = if wait {
        libc::F_OFD_SETLKW
    } else {
        libc::F_OFD_SETLK
    };
    #[cfg(not(target_os = "linux"))]
    let command = if wait { libc::F_SETLKW } else { libc::F_SETLK };
    // SAFETY: a zeroed flock is valid; l_pid must be 0 for an OFD lock.
    let mut whole: libc::flock = unsafe { std::mem::zeroed() };
    whole.l_type = kind as _;
    whole.l_whence = libc::SEEK_SET as _;
    loop {
        // SAFETY: `whole` outlives the call, and the descriptor is open.
        if unsafe { libc::fcntl(file.as_raw_fd(), command, &whole) } == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// The folder at `path`, as its lock file records it.
fn identity(path: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let found = std::fs::symlink_metadata(path).ok()?;
    found
        .is_dir()
        .then(|| format!("{} {}", found.dev(), found.ino()))
}

/// A placeholder held: its path and its lock file, read-locked.
struct Held {
    path: PathBuf,
    lock: std::fs::File,
}

impl Held {
    /// Read-lock the placeholder at `path`, waiting out a removal, and make
    /// it when its folder is there to make it in, recording it as this
    /// sandbox's. `None` when its lock can't be taken: then it is never
    /// removed.
    fn take(path: &Path) -> Option<Held> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(lock_file(path)?)
            .ok()?;
        self::lock(&lock, libc::F_RDLCK as _, true).ok()?;
        // Made here rather than by bubblewrap, so it is known for this
        // sandbox's. Only one maker succeeds, and only it writes.
        if std::fs::create_dir(path).is_ok()
            && let Some(made) = identity(path)
        {
            let _ = lock
                .set_len(0)
                .and_then(|()| lock.write_all(made.as_bytes()));
        }
        Some(Held {
            path: path.to_owned(),
            lock,
        })
    }

    /// Let go; the last holder removes the placeholder when it is still
    /// the empty folder made for one.
    fn free(self) {
        use std::io::{Read, Seek};
        let Held { path, mut lock } = self;
        // Its own read lock goes first. Two holders letting go at once,
        // each asking for the write lock while still holding its read
        // lock, would each be refused for the other's and both leave it.
        // Let go first, the write lock is refused only while another holds
        // on, and that one asks in turn as it goes.
        let _ = self::lock(&lock, libc::F_UNLCK as _, false);
        if self::lock(&lock, libc::F_WRLCK as _, false).is_err() {
            return;
        }
        let mut made = String::new();
        let _ = lock
            .seek(std::io::SeekFrom::Start(0))
            .and_then(|_| lock.read_to_string(&mut made));
        let now = identity(&path);
        if now.is_none()
            || now.as_deref() == Some(made.as_str()) && std::fs::remove_dir(&path).is_ok()
        {
            let _ = lock.set_len(0);
        }
        // Closing the file lets the lock go.
    }
}

/// When a sandbox is gone: on Linux, the read end of a pipe whose write
/// end bubblewrap's init keeps (`--sync-fd`), and once the command has
/// started, it alone, so the pipe reads to its end when init is gone.
/// Init's locks on the placeholders go before that: the kernel lets a
/// process's record locks go as it closes each of its files, and only
/// after it has closed them all finishes the pipe's close. `None` where
/// nothing of the sandbox outlives its command.
pub struct Ended(Option<std::os::fd::OwnedFd>);

impl Ended {
    /// For a command run unconfined.
    pub fn none() -> Ended {
        Ended(None)
    }
}

/// Whether the sandbox `end` tells of is gone: its pipe at its end, as
/// nothing is ever written to it. Asked without waiting.
fn gone(end: &std::os::fd::OwnedFd) -> bool {
    use std::os::fd::AsRawFd;
    let mut asked = libc::pollfd {
        fd: end.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one pollfd, valid for the call, which doesn't wait.
    (unsafe { libc::poll(&mut asked, 1, 0) }) == 1
        && asked.revents & (libc::POLLIN | libc::POLLHUP) != 0
}

/// What a running sandbox holds in place; dropping it lets the
/// placeholders go once no other sandbox holds them, removes what the
/// command may not leave made, and gives up its share in the bot's socket,
/// each once the sandboxes it [`waits for`](Hold::waits_for) are gone.
pub struct Hold(
    Vec<Held>,
    Vec<PathBuf>,
    // Held only to be dropped with the rest.
    #[allow(dead_code)] Option<Listening>,
    Vec<std::os::fd::OwnedFd>,
);

/// Hold `policy`'s placeholders while its command runs, once any of them
/// being removed is gone. Off the runtime thread: it opens and locks files.
/// On Linux it also makes again, private as the daemon made it at start,
/// each walled folder something removed since: bubblewrap mounts over every
/// one and has no mount point to make on a read-only root, and a command
/// left without the wall would reach the sockets of one made again while it
/// runs. Each folder it may not read that is gone is made the same way: on
/// Linux it is masked where it is, and one made while the command ran
/// would be open to it. One that cannot be made is left to fail its
/// command, closed.
pub async fn hold(policy: &Policy) -> Hold {
    let (paths, walled): (Vec<PathBuf>, Vec<PathBuf>) = match cfg!(target_os = "linux") {
        true => (
            policy
                .roots
                .iter()
                .flat_map(|root| root.missing.iter().cloned())
                .collect(),
            policy.walled.clone(),
        ),
        false => (Vec::new(), Vec::new()),
    };
    let hidden = policy.unreadable.clone();
    let left = policy
        .roots
        .iter()
        .flat_map(|root| root.left.iter().cloned())
        .collect();
    let held = match paths.is_empty() && walled.is_empty() && hidden.is_empty() {
        true => Vec::new(),
        false => tokio::task::spawn_blocking(move || {
            use std::os::unix::fs::DirBuilderExt;
            for folder in (walled.iter().chain(&hidden)).filter(|folder| !folder.is_dir()) {
                let _ = std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(folder);
            }
            paths.iter().filter_map(|path| Held::take(path)).collect()
        })
        .await
        .unwrap_or_default(),
    };
    Hold(held, left, policy.listening.clone(), Vec::new())
}

/// [`hold`] for a call that may run unconfined.
pub async fn hold_for(policy: Option<&Policy>) -> Option<Hold> {
    match policy {
        Some(policy) => Some(hold(policy).await),
        None => None,
    }
}

impl Hold {
    /// Let go only once `ended`'s sandbox is gone. Its command's end isn't
    /// enough: bubblewrap's init, which holds the placeholders' locks too,
    /// dies only after it, and a placeholder let go while init still holds
    /// it is never removed. The caller drops its copy of the command first,
    /// with the pipe's write end in it. One held over many sandboxes, as a
    /// patch's over its writes, keeps the pipes of only those still running.
    pub fn waits_for(&mut self, ended: Ended) {
        self.3.retain(|end| !gone(end));
        self.3.extend(ended.0);
    }

    /// Let go now, on this thread, for a process that exits next, as
    /// `agent sandbox` does: a drop removes the placeholders on a thread of
    /// its own, which would end with the process, its work undone.
    pub fn release(mut self) {
        self.free(true);
    }

    fn free(&mut self, here: bool) {
        let held = std::mem::take(&mut self.0);
        let left = std::mem::take(&mut self.1);
        let ends = std::mem::take(&mut self.3);
        if held.is_empty() && left.is_empty() {
            return;
        }
        // Off the runtime thread. On a thread of its own, since a runtime
        // shutting down drops blocking work not yet started, which would
        // leave them in place.
        let remove = move || {
            // Nothing is ever written to the pipe: it reads to its end once
            // the sandbox is gone.
            for end in ends {
                let _ = std::io::copy(&mut std::fs::File::from(end), &mut std::io::sink());
            }
            // Git never makes these in a repository's own folder: one there
            // now is the command's.
            for path in &left {
                let _ = std::fs::remove_file(path);
            }
            for held in held {
                held.free();
            }
        };
        if here {
            return remove();
        }
        type Job = std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>;
        let run = |job: &Job| {
            if let Some(remove) = job.lock().unwrap().take() {
                remove();
            }
        };
        let job: std::sync::Arc<Job> =
            std::sync::Arc::new(std::sync::Mutex::new(Some(Box::new(remove))));
        let theirs = job.clone();
        if std::thread::Builder::new()
            .name("sandbox-placeholders".into())
            .spawn(move || run(&theirs))
            .is_err()
        {
            run(&job);
        }
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        self.free(false);
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::Policy;
    use crate::Result;

    /// By its absolute path, as Codex runs it, so PATH can't name another.
    pub(super) const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

    pub(super) fn command(
        policy: &Policy,
        program: &str,
        rest: &[&str],
    ) -> Result<tokio::process::Command> {
        let (mut profile, params) = super::seatbelt(policy);
        let mut command = tokio::process::Command::new(SANDBOX_EXEC);
        if !policy.domains.is_empty() {
            let port = super::loopback_proxy(&policy.domains)?;
            profile = super::proxied(profile, port);
            super::through_proxy(policy, &mut command)?;
        }
        command.arg("-p").arg(profile);
        for (key, value) in params {
            command.arg(format!("-D{key}={}", value.to_string_lossy()));
        }
        command.arg("--").arg(program).args(rest);
        Ok(command)
    }
}

/// The port on this machine's loopback where the proxy serves the
/// commands whose bots name `domains`, as Codex's runs on one: on macOS,
/// where Seatbelt lets a command reach that port and nothing else. One for
/// each list, for as long as the daemon runs. Others that reach loopback
/// reach it too, and with it no more than the list lets through.
pub fn loopback_proxy(domains: &BTreeMap<String, proxy::Access>) -> Result<u16> {
    type Listeners = std::sync::Mutex<BTreeMap<BTreeMap<String, proxy::Access>, u16>>;
    static LISTENING: std::sync::LazyLock<Listeners> = std::sync::LazyLock::new(Default::default);
    let mut listening = LISTENING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(port) = listening.get(domains) {
        return Ok(*port);
    }
    let runtime = tokio::runtime::Handle::try_current().map_err(|_| {
        crate::Error::with(
            "sandbox_unavailable",
            "domains need the daemon's runtime, which answers the proxy",
        )
    })?;
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let given = std::sync::Arc::new(domains.clone());
    runtime.spawn(async move {
        let Ok(listener) = tokio::net::TcpListener::from_std(listener) else {
            return;
        };
        loop {
            if let Ok((client, _)) = listener.accept().await {
                tokio::spawn(proxy::serve(client, given.clone()));
            }
        }
    });
    listening.insert(domains.clone(), port);
    Ok(port)
}

/// `profile`, its commands also reaching the proxy at `port` on loopback,
/// and the services a TLS client asks to trust what it reaches there.
pub fn proxied(profile: String, port: u16) -> String {
    format!(
        "{profile}\n(allow network-outbound (remote ip \"localhost:{port}\"))\n\
         {SEATBELT_NETWORK}{SEATBELT_TLS_TRUST}"
    )
}

/// Codex's Seatbelt policy pieces, as Codex ships them.
const SEATBELT_BASE: &str = include_str!("sandbox/seatbelt_base_policy.sbpl");
const SEATBELT_NETWORK: &str = include_str!("sandbox/seatbelt_network_policy.sbpl");
const SEATBELT_PREFERENCES: &str = include_str!("sandbox/seatbelt_preferences_policy.sbpl");
const SEATBELT_TLS_TRUST: &str =
    "(allow mach-lookup (global-name \"com.apple.TrustEvaluationAgent\"))\n";

/// A rule that keeps the folder `key` names from being renamed or removed.
fn anchor(key: &str) -> String {
    format!(
        "(deny file-write-unlink (require-all (literal (param \"{key}\")) (vnode-type DIRECTORY)))"
    )
}

/// The Seatbelt profile for `policy` and the parameters it names, as Codex
/// writes it: everything denied but what its base allows, reading open,
/// writing each root but the paths it keeps, and network only when on.
/// Paths go in as parameters, never into the profile's text.
pub fn seatbelt(policy: &Policy) -> (String, Vec<(String, PathBuf)>) {
    let mut params: Vec<(String, PathBuf)> = Vec::new();
    // One parameter per path, however many rules name it.
    let mut param = |path: &Path| -> String {
        if let Some((key, _)) = params.iter().find(|(_, named)| named == path) {
            return key.clone();
        }
        let key = format!("PATH_{}", params.len());
        params.push((key.clone(), path.to_owned()));
        key
    };
    let mut allows = Vec::new();
    let mut anchors = Vec::new();
    for (index, root) in policy.roots.iter().enumerate() {
        let key = param(&root.path);
        // The root itself can't be renamed away from under its own rules.
        anchors.push(anchor(&key));
        // What this root keeps, and the deeper roots just inside it, which
        // decide for themselves: the deepest root decides, as bubblewrap's
        // mounts do, and each rule names only what is its own.
        let inner = policy.roots[index + 1..].iter().filter(|inner| {
            inner.path.starts_with(&root.path)
                && !policy.roots[index + 1..].iter().any(|between| {
                    between.path != inner.path
                        && between.path.starts_with(&root.path)
                        && inner.path.starts_with(&between.path)
                })
        });
        let mut parts = vec![format!("(subpath (param \"{key}\"))")];
        for kept in root
            .read_only
            .iter()
            .chain(&root.missing)
            .chain(&root.others)
        {
            let kept_key = param(kept);
            // Both the path and what is under it: `subpath` alone would let
            // the folder itself be made.
            parts.push(format!("(require-not (literal (param \"{kept_key}\")))"));
            parts.push(format!("(require-not (subpath (param \"{kept_key}\")))"));
        }
        for inner in inner {
            let inner_key = param(&inner.path);
            parts.push(format!("(require-not (subpath (param \"{inner_key}\")))"));
        }
        allows.push(match parts.len() {
            1 => parts.remove(0),
            _ => format!("(require-all {} )", parts.join(" ")),
        });
    }
    // Nor can the folders between a root and what stays out of reach (a
    // kept path, the daemon's socket): renamed, they would take it to a
    // name its rule doesn't cover.
    let mut folders: Vec<&Path> = policy
        .roots
        .iter()
        .flat_map(|root| {
            root.read_only
                .iter()
                .chain(&root.missing)
                .chain(&root.others)
        })
        .chain(&policy.socket)
        .chain(&policy.walled)
        .chain(&policy.daemons)
        .chain(&policy.unreadable)
        .flat_map(|kept| kept.ancestors().skip(1))
        .filter(|folder| {
            policy
                .roots
                .iter()
                .any(|root| folder.starts_with(&root.path))
                && !policy.roots.iter().any(|root| root.path == *folder)
        })
        .collect();
    folders.sort();
    folders.dedup();
    for folder in folders {
        anchors.push(anchor(&param(folder)));
    }
    let mut sections = vec![
        SEATBELT_BASE.to_owned(),
        "; allow read-only file operations\n(allow file-read*)".to_owned(),
    ];
    if !allows.is_empty() {
        sections.push(format!("(allow file-write*\n{}\n)", allows.join(" ")));
        sections.extend(anchors);
    }
    // Network as Codex allows it: everything when on, nothing when off.
    if policy.network {
        sections.push(format!(
            "(allow network-outbound)\n(allow network-inbound)\n{SEATBELT_NETWORK}{SEATBELT_TLS_TRUST}"
        ));
    }
    // Unix sockets every one with the network on, as Codex's full network
    // reaches them, and its own it listens on, as a test's server does;
    // with it off, only those its settings name.
    if policy.network {
        sections.push(
            "(allow system-socket (socket-domain AF_UNIX))\n\
             (allow network-bind (local unix-socket))"
                .to_owned(),
        );
    } else if !policy.sockets.is_empty() {
        let mut rules = vec!["(allow system-socket (socket-domain AF_UNIX))".to_owned()];
        for path in &policy.sockets {
            let key = param(path);
            rules.push(format!(
                "(allow network-bind (local unix-socket (subpath (param \"{key}\"))))\n\
                 (allow network-outbound (remote unix-socket (subpath (param \"{key}\"))))"
            ));
        }
        sections.push(rules.join("\n"));
    }
    // Never a walled folder's, whose programs act as the person, by any
    // name: after what the network or the settings let it reach.
    for folder in &policy.walled {
        let key = param(folder);
        sections.push(format!(
            "(deny network-outbound (remote unix-socket (subpath (param \"{key}\"))))\n\
             (deny file-link (subpath (param \"{key}\")))"
        ));
    }
    // Never the daemon's socket itself: the bot's own instead, which
    // answers as that bot and only as far as a sandboxed bot may go. Nor
    // by a hard link to it in a folder the command may write, which would
    // reach it by another name.
    if let Some(socket) = &policy.socket {
        let key = param(socket);
        sections.push(format!(
            "(deny network-outbound (remote unix-socket (subpath (param \"{key}\"))))\n\
             (deny file-link (subpath (param \"{key}\")))"
        ));
    }
    // Nor any other daemon's, by any name, nor its bots' own sockets.
    for socket in &policy.daemons {
        for path in [socket.clone(), bots_folder(socket)] {
            let key = param(&path);
            sections.push(format!(
                "(deny network-outbound (remote unix-socket (subpath (param \"{key}\"))))\n\
                 (deny file-link (subpath (param \"{key}\")))"
            ));
        }
    }
    if let Some(own) = &policy.capability {
        // Not another bot's, which would answer as its own bot.
        if let Some(folder) = own.parent() {
            let key = param(folder);
            sections.push(format!(
                "(deny network-outbound (remote unix-socket (subpath (param \"{key}\"))))\n\
                 (deny file-link (subpath (param \"{key}\")))"
            ));
        }
        let key = param(own);
        sections.push(format!(
            "(allow system-socket (socket-domain AF_UNIX))\n\
             (allow network-outbound (remote unix-socket (subpath (param \"{key}\"))))"
        ));
    }
    // Never what it may not read, nor written, made or linked to under
    // another name, as Codex denies its deny_read paths: after every allow,
    // so the deny wins.
    for hidden in &policy.unreadable {
        let key = param(hidden);
        sections.push(format!(
            "(deny file-read* (subpath (param \"{key}\")))\n\
             (deny file-write* (subpath (param \"{key}\")))\n\
             (deny file-link (subpath (param \"{key}\")))"
        ));
    }
    sections.push(SEATBELT_PREFERENCES.to_owned());
    sections.push("(deny mach-lookup (xpc-service-name-prefix \"\"))".to_owned());
    // These fcntls change a file through a read-only descriptor, past
    // file-write*: F_MAKECOMPRESSED and F_TRANSFEREXTENTS.
    sections.push("(deny system-fcntl (fcntl-command 80 110))".to_owned());
    (sections.join("\n"), params)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domains_turn_the_network_off_but_for_the_proxy() {
        let domains: BTreeMap<String, proxy::Access> =
            [("**.example.com".to_owned(), proxy::Access::Allow)].into();
        let daemon = Daemon::default();
        let open = || {
            Policy::new(Some(Mode::WorkspaceWrite), None, &[], true, &daemon)
                .unwrap()
                .unwrap()
        };
        let proxied = open().with_domains(&domains);
        assert!(!proxied.network);
        assert_eq!(proxied.domains, domains);
        // None named: the whole network, as before; off, it stays off.
        assert!(open().with_domains(&BTreeMap::new()).network);
        let off = Policy::new(Some(Mode::WorkspaceWrite), None, &[], false, &daemon)
            .unwrap()
            .unwrap()
            .with_domains(&domains);
        assert!(!off.network && off.domains.is_empty());
        // On macOS its commands reach the proxy's port, and only it.
        let (profile, _) = seatbelt(&proxied);
        assert!(!profile.contains("(allow network-outbound)"));
        let profile = super::proxied(profile, 4321);
        assert!(profile.contains("(allow network-outbound (remote ip \"localhost:4321\"))"));
        assert!(!profile.contains("(allow network-outbound)\n"));
        let set = proxy_env("http://127.0.0.1:4321");
        assert!(set.contains(&("HTTPS_PROXY", "http://127.0.0.1:4321".to_owned())));
        assert!(set.contains(&("https_proxy", "http://127.0.0.1:4321".to_owned())));
    }

    /// On macOS a command's bot can't point it past its proxy: the proxy's
    /// variables are set over its own, and no exclusion is kept.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_command_s_own_no_proxy_never_sends_it_past_its_proxy() {
        let domains: BTreeMap<String, proxy::Access> =
            [("only.example".to_owned(), proxy::Access::Allow)].into();
        let daemon = Daemon::default();
        let policy = Policy::new(Some(Mode::WorkspaceWrite), None, &[], true, &daemon)
            .unwrap()
            .unwrap()
            .with_domains(&domains);
        let mut command = tokio::process::Command::new("true");
        command
            .env("NO_PROXY", "*")
            .env("HTTPS_PROXY", "http://elsewhere:1");
        through_proxy(&policy, &mut command).unwrap();
        let envs: BTreeMap<_, _> = command.as_std().get_envs().collect();
        assert_eq!(envs[std::ffi::OsStr::new("NO_PROXY")], None);
        let port = loopback_proxy(&domains).unwrap();
        assert_eq!(
            envs[std::ffi::OsStr::new("HTTPS_PROXY")],
            Some(std::ffi::OsStr::new(&format!("http://127.0.0.1:{port}")))
        );
    }

    /// The proxy on loopback serves one list, the same port each time.
    #[tokio::test]
    async fn one_loopback_proxy_serves_each_list() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let domains: BTreeMap<String, proxy::Access> =
            [("only.example".to_owned(), proxy::Access::Allow)].into();
        let port = loopback_proxy(&domains).unwrap();
        assert_eq!(loopback_proxy(&domains).unwrap(), port);
        let mut client = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        client
            .write_all(b"CONNECT elsewhere.example:443 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut answer = String::new();
        client.read_to_string(&mut answer).await.unwrap();
        assert!(
            answer.contains("x-proxy-error: blocked-by-allowlist"),
            "{answer}"
        );
    }

    #[test]
    fn temporary_room_reads_each_temporary_file_system_once() {
        let rooms = temporary_room();
        // /tmp always exists here; $TMPDIR may be on the same file system.
        assert!(!rooms.is_empty());
        for room in &rooms {
            assert!(room.path.is_absolute());
            assert!(room.bytes_total > 0 && room.bytes_free <= room.bytes_total);
        }
        let real: Vec<PathBuf> = (rooms.iter())
            .map(|room| std::fs::canonicalize(&room.path).unwrap())
            .collect();
        for (at, path) in real.iter().enumerate() {
            assert!(!real[..at].contains(path), "{path:?} read twice");
        }
    }

    /// The daemon as the CLI starts it, protecting what its client names.
    fn protecting() -> Daemon {
        Daemon {
            protected: agent_client::PROTECTED.map(String::from).to_vec(),
            protected_existing: agent_client::PROTECTED_EXISTING.map(String::from).to_vec(),
            ..Daemon::default()
        }
    }

    /// Another daemon of the person's, listed where daemons list
    /// themselves, is kept as this one is: its store and the files beside
    /// it read-only, its socket and its bots' sockets out of reach, the
    /// network on or off, wherever an agent put them.
    #[test]
    fn every_other_daemon_s_store_and_socket_are_kept_as_its_own() {
        let base =
            std::env::temp_dir().join(format!("agent-sandbox-others-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("work")).unwrap();
        let base = std::fs::canonicalize(&base).unwrap();
        let work = base.join("work");
        // A side daemon an agent started in the folder it works in.
        let (store, socket) = (work.join("side.sqlite"), work.join("side.sock"));
        // Beside its socket, or in a short folder where a long temporary
        // folder leaves no room for a bot's socket address there.
        let bots = bots_folder(&socket);
        std::fs::create_dir_all(&bots).unwrap();
        std::fs::write(&store, "").unwrap();
        std::fs::write(&socket, "").unwrap();
        let folder = base.join("daemons");
        let own = base.join("main.sqlite");
        std::fs::write(&own, "").unwrap();
        for (store, socket) in [(&own, base.join("main.sock")), (&store, socket.clone())] {
            let listed = known::Known {
                store: store.clone(),
                socket: Some(socket),
            };
            known::record(&folder, &listed).unwrap();
        }
        let daemon = Daemon {
            store: Some(own.clone()),
            socket: Some(base.join("main.sock")),
            known: Some(folder.clone()),
            ..protecting()
        };
        let policy = Policy::new(Some(Mode::WorkspaceWrite), Some(&work), &[], true, &daemon)
            .unwrap()
            .unwrap();
        // Its own daemon is not another.
        assert_eq!(policy.daemons, std::slice::from_ref(&socket));
        let root = policy.roots.iter().find(|root| root.path == work).unwrap();
        for kept in [store.clone(), work.join("side.sqlite-wal"), socket.clone()] {
            assert!(root.others.contains(&kept), "{}", kept.display());
            assert!(!policy.can_write(&kept), "{}", kept.display());
        }
        assert!(policy.can_write(&work.join("notes.md")));
        let (profile, params) = seatbelt(&policy);
        let key = |path: &Path| {
            let (key, _) = params.iter().find(|(_, named)| named == path).unwrap();
            key.clone()
        };
        for hidden in [socket.clone(), bots.clone()] {
            let denied = format!(
                "(deny network-outbound (remote unix-socket (subpath (param \"{}\"))))",
                key(&hidden)
            );
            assert!(
                profile.find(&denied).unwrap()
                    > profile.find("(allow network-outbound)\n").unwrap()
            );
        }
        #[cfg(target_os = "linux")]
        {
            let args = linux::args(&policy, false);
            let has = |a: &str, b: &Path, c: &Path| {
                args.windows(3).any(|w| {
                    w[0] == a && w[1] == b.to_string_lossy() && w[2] == c.to_string_lossy()
                })
            };
            assert!(has("--ro-bind", Path::new("/dev/null"), &socket));
            assert!(has("--ro-bind-try", &store, &store));
            // Its journals, absent while it is stopped, are there to be
            // kept, empty, so none can be made for SQLite to play back.
            for journal in ["-wal", "-shm", "-journal"] {
                let kept = work.join(format!("side.sqlite{journal}"));
                assert_eq!(std::fs::read(&kept).unwrap(), b"", "{}", kept.display());
                assert!(has("--ro-bind-try", &kept, &kept));
            }
            assert!(
                args.windows(2)
                    .any(|w| w[0] == "--tmpfs" && w[1] == bots.to_string_lossy())
            );
            // A daemon that stopped took its socket: nothing to hide, and
            // nothing made where it was, which a read-only root couldn't
            // hold; what is kept only while there is skipped by bubblewrap.
            std::fs::remove_file(&socket).unwrap();
            std::fs::remove_dir(&bots).unwrap();
            let args = linux::args(&policy, false);
            assert!(
                !args
                    .windows(3)
                    .any(|w| w[0] == "--ro-bind" && w[2] == socket.to_string_lossy())
            );
            assert!(
                !args
                    .windows(2)
                    .any(|w| w[0] == "--tmpfs" && w[1] == bots.to_string_lossy())
            );
        }
        // Where it is outside the test's folder.
        let _ = std::fs::remove_dir(&bots);
        std::fs::remove_dir_all(base).unwrap();
    }

    /// A bot whose folder holds the agent's home (Sonny opened in HOME)
    /// leaves it alone, the scheduler's jobs and the settings with it, but
    /// for a folder in it the bot was given by name.
    #[test]
    fn a_protected_home_stays_read_only_but_for_a_folder_given_in_it() {
        let base = std::env::temp_dir().join(format!("agent-sandbox-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let home = base.join(".agent");
        std::fs::create_dir_all(home.join("automations/jobs")).unwrap();
        std::fs::create_dir_all(home.join("sonny/zen")).unwrap();
        std::fs::write(home.join("config.toml"), "").unwrap();
        let base = std::fs::canonicalize(&base).unwrap();
        let home = base.join(".agent");
        let zen = home.join("sonny/zen");
        let daemon = Daemon {
            protected_paths: vec![home.clone()],
            ..protecting()
        };
        let policy = Policy::new(
            Some(Mode::WorkspaceWrite),
            Some(&base),
            &[zen.to_str().unwrap().to_owned()],
            false,
            &daemon,
        )
        .unwrap()
        .unwrap();
        assert!(policy.can_write(&base.join("notes.txt")));
        assert!(!policy.can_write(&home.join("automations/jobs/nightly.json")));
        assert!(!policy.can_write(&home.join("config.toml")));
        assert!(!policy.can_write(&home.join("env")));
        assert!(policy.can_write(&zen.join("key")));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// What the daemon keeps unreadable no command reads, writes, links to
    /// or renames, in its roots or out of them, by any link to it, nor
    /// anything in it; the rest of what it may read and write is as it was.
    #[test]
    fn an_unreadable_folder_is_closed_to_every_command() {
        let base = std::env::temp_dir().join(format!("agent-sandbox-hide-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("work/keys")).unwrap();
        std::fs::create_dir_all(base.join("home/keys")).unwrap();
        std::fs::write(base.join("home/keys/env"), "KEY=x").unwrap();
        let base = std::fs::canonicalize(&base).unwrap();
        let (work, home, keys) = (
            base.join("work"),
            base.join("home/keys"),
            base.join("work/keys"),
        );
        std::os::unix::fs::symlink(&home, work.join("linked")).unwrap();
        // A folder shown from the person's copy that holds one, or is in
        // one, isn't copied: the copy would show it at a name of its own.
        let daemon = Daemon {
            unreadable: vec![home.clone(), keys.clone()],
            owned: vec![base.join("home"), home.join("sub")],
            ..protecting()
        };
        let policy = Policy::new(Some(Mode::WorkspaceWrite), Some(&work), &[], true, &daemon)
            .unwrap()
            .unwrap();
        assert!(policy.owned.is_empty());
        for hidden in [
            &home,
            &home.join("env"),
            &work.join("linked/env"),
            &keys,
            &keys.join("id"),
        ] {
            assert!(!policy.can_read(hidden), "{}", hidden.display());
            assert!(!policy.can_write(hidden), "{}", hidden.display());
        }
        assert!(!policy.can_remove(&keys));
        assert!(policy.can_read(&base.join("home")) && policy.can_read(&work.join("notes")));
        assert!(policy.can_write(&work.join("notes")));
        let (profile, params) = seatbelt(&policy);
        for hidden in [&home, &keys] {
            let (key, _) = params.iter().find(|(_, path)| path == hidden).unwrap();
            for operation in ["file-read*", "file-write*", "file-link"] {
                let rule = format!("(deny {operation} (subpath (param \"{key}\")))");
                assert!(profile.contains(&rule), "{rule}");
            }
        }
        // After the allow it overrides.
        let allow = profile.find("(allow file-read*)").unwrap();
        assert!(profile.rfind("(deny file-read*").unwrap() > allow);
        #[cfg(target_os = "linux")]
        {
            // Closed over every other mount, the person's copies too.
            let mut policy = policy.clone();
            policy.owned = vec![(base.join("copy"), keys.clone())];
            let args = linux::args(&policy, false);
            for hidden in [&home, &keys] {
                let hidden = hidden.to_string_lossy();
                let mask = args
                    .windows(4)
                    .position(|w| w[0] == "000" && w[1] == "--tmpfs" && w[2] == hidden)
                    .unwrap();
                assert!(!args[mask..].iter().any(|arg| arg == "--ro-bind"));
            }
        }
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_call_escapes_by_asking_or_by_writing_outside_its_roots() {
        let root = Path::new("/work/project");
        let escapes = |tool: &str, arguments: serde_json::Value| {
            escapes(
                Mode::WorkspaceWrite,
                Some(root),
                &["/work/shared".to_owned()],
                tool,
                &arguments.to_string(),
                Path::new("/work/project/sub"),
                &Links::default(),
            )
        };
        assert!(!escapes("shell", serde_json::json!({"command":"make"})));
        assert!(!escapes(
            "shell",
            serde_json::json!({"command":"make","sandbox_permissions":"use_default"})
        ));
        assert!(escapes(
            "shell",
            serde_json::json!({"command":"git push","sandbox_permissions":"require_escalated"})
        ));
        for inside in ["a.txt", "../b.txt", "/work/shared/c", "/tmp/d"] {
            assert!(
                !escapes("write", serde_json::json!({"path":inside,"content":""})),
                "{inside}"
            );
        }
        for outside in ["../../elsewhere", "/etc/hosts", "/work/projectile/x"] {
            assert!(
                escapes(
                    "edit",
                    serde_json::json!({"path":outside,"old":"a","new":"b"})
                ),
                "{outside}"
            );
        }
        let patch = |paths: &[&str]| {
            let mut input = String::from("*** Begin Patch\n");
            for path in paths {
                input.push_str(&format!("*** Add File: {path}\n+x\n"));
            }
            input.push_str("*** End Patch");
            serde_json::json!({ "input": input })
        };
        assert!(!escapes("apply_patch", patch(&["a.txt", "/work/shared/c"])));
        assert!(escapes("apply_patch", patch(&["a.txt", "/etc/hosts"])));
        assert!(!escapes(
            "apply_patch",
            serde_json::json!({"input":"not a patch"})
        ));
        // A link the bot can't change is followed: a home reached through
        // one is the folder it names. One in a folder the bot writes is
        // not, so a write through it is judged by its spelling.
        let base = std::env::temp_dir().join(format!("agent-escapes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let real = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("agent-escapes-{}", std::process::id()));
        let home = real.join("home/person");
        let project = home.join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(real.join("fixed")).unwrap();
        std::os::unix::fs::symlink(&home, real.join("fixed/me")).unwrap();
        std::os::unix::fs::symlink("../../../fixed", project.join("out")).unwrap();
        let fixed = |dir: &Path| dir.starts_with(&project);
        // Judging reads no file: a link not read for the call leaves the
        // path unjudged, so it is taken as spelled.
        let unread = real.join("fixed/me/project/a.txt");
        assert_eq!(judged(&unread, fixed, |_| None, false), None);
        let followed = |path: &Path, removed| {
            // The test's folders are in the temporary folder, which every
            // bot writes: judged as if its links were in a fixed folder.
            judged(path, fixed, |path| Some(read_link(path)), removed).unwrap()
        };
        assert_eq!(followed(&unread, false), project.join("a.txt"));
        // A `..` after a followed link reads from where it led.
        assert_eq!(
            followed(&real.join("fixed/me/../person/project/b"), false),
            project.join("b")
        );
        // Through a link in the project, as spelled.
        assert_eq!(
            followed(&project.join("out/me/x"), false),
            project.join("out/me/x")
        );
        // A removal takes the link itself, not where it leads.
        assert_eq!(
            followed(&real.join("fixed/me"), true),
            real.join("fixed/me")
        );
        assert_eq!(followed(&real.join("fixed/me"), false), home);
        // What was read goes with the call: a link changed since changes
        // nothing a call already read judges by, and a call read later
        // sees the change.
        let arguments = serde_json::json!({"path": unread, "content": ""}).to_string();
        let learn = || {
            super::learn(
                Mode::ReadOnly,
                None,
                &[],
                "write",
                &arguments,
                Path::new("/"),
            )
        };
        let given = unread.to_str().unwrap();
        let link = |links: &Links| links.target(given, false).cloned();
        let first = learn();
        assert!(first.read() && !Links::default().read());
        assert_eq!(link(&first), Some(project.join("a.txt")));
        // One entry per path the call names, whatever it passed.
        assert_eq!(first.0.as_deref().map(|links| links.len()), Some(1));
        // Kept with a parked call, it comes back as read.
        let kept: Links = serde_json::from_str(&serde_json::to_string(&first).unwrap()).unwrap();
        assert_eq!(link(&kept), Some(project.join("a.txt")));
        assert!(serde_json::from_str::<Links>("null").unwrap().unread());
        std::fs::remove_file(real.join("fixed/me")).unwrap();
        std::os::unix::fs::symlink(real.join("fixed"), real.join("fixed/me")).unwrap();
        assert_eq!(link(&first), Some(project.join("a.txt")));
        assert_eq!(link(&learn()), Some(real.join("fixed/project/a.txt")));
        // A removal is kept both as the entry and as what it leads to.
        let gone = real.join("fixed/me");
        let patch = format!(
            "*** Begin Patch\n*** Delete File: {}\n*** End Patch",
            gone.display()
        );
        let removal = super::learn(
            Mode::ReadOnly,
            None,
            &[],
            "apply_patch",
            &serde_json::json!({ "input": patch }).to_string(),
            Path::new("/"),
        );
        let given = gone.to_str().unwrap();
        assert_eq!(removal.target(given, true), Some(&gone));
        assert_eq!(removal.target(given, false), Some(&real.join("fixed")));
        // A path no tool takes is never read.
        assert!(judgeable(&"a".repeat(4096)));
        assert!(!judgeable(&"a".repeat(4097)));
        assert!(!judgeable("a\0b"));
        let _ = std::fs::remove_dir_all(&base);
        // Read-only writes nowhere, so every write asks.
        assert!(super::escapes(
            Mode::ReadOnly,
            Some(root),
            &[],
            "write",
            r#"{"path":"a.txt","content":""}"#,
            root,
            &Links::default()
        ));
    }

    /// A system folder whose files a sandbox would show as nobody's is
    /// copied as the person's, kept in step with it, and bound in its place.
    #[test]
    fn a_system_folder_is_bound_from_the_person_s_copy() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("agent-sandbox-owned-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let place = base.join("etc/ssh/ssh_config.d");
        let copy = base.join("copies/etc/ssh/ssh_config.d");
        std::fs::create_dir_all(place.join("folder")).unwrap();
        std::fs::write(place.join("a.conf"), "Host a\n").unwrap();
        std::fs::write(place.join("b.conf"), "Host b\n").unwrap();
        assert!(copied(&place, &copy));
        let read = |name: &str| std::fs::read_to_string(copy.join(name)).ok();
        assert_eq!(read("a.conf").as_deref(), Some("Host a\n"));
        assert_eq!(read("b.conf").as_deref(), Some("Host b\n"));
        let mode = std::fs::metadata(copy.join("a.conf"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        // Only files are copied, and only what is there.
        let names = |dir: &Path| {
            let mut names: Vec<String> = (std::fs::read_dir(dir).unwrap().flatten())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        };
        assert_eq!(names(&copy), ["a.conf", "b.conf"]);
        // A file changed, one gone, one new: copied again.
        std::fs::write(place.join("a.conf"), "Host changed\n").unwrap();
        std::fs::remove_file(place.join("b.conf")).unwrap();
        std::fs::write(place.join("c.conf"), "Host c\n").unwrap();
        assert!(copied(&place, &copy));
        assert_eq!(read("a.conf").as_deref(), Some("Host changed\n"));
        assert_eq!(names(&copy), ["a.conf", "c.conf"]);
        // A copy taken away is made again, though nothing changed.
        std::fs::remove_dir_all(&copy).unwrap();
        assert!(copied(&place, &copy));
        assert_eq!(names(&copy), ["a.conf", "c.conf"]);
        // A folder that isn't there has no copy.
        assert!(!copied(&base.join("gone"), &base.join("copies/gone")));
        // Another process copying holds the lock beside the folder, out of
        // what a sandbox sees: this one waits for it.
        let held = std::fs::File::open(base.join("copies/etc/ssh/ssh_config.d.lock")).unwrap();
        held.lock().unwrap();
        let wrote = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let waiting = {
            let (copy, wrote) = (copy.clone(), wrote.clone());
            std::thread::spawn(move || {
                locked(&copy, || {
                    wrote.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                })
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!wrote.load(std::sync::atomic::Ordering::SeqCst));
        drop(held);
        waiting.join().unwrap().unwrap();
        assert!(wrote.load(std::sync::atomic::Ordering::SeqCst));
        #[cfg(target_os = "linux")]
        {
            let policy = Policy {
                owned: vec![(copy.clone(), place.clone())],
                ..Policy::new(Some(Mode::ReadOnly), None, &[], false, &Daemon::default())
                    .unwrap()
                    .unwrap()
            };
            let args = linux::args(&policy, false);
            assert!(args.windows(3).any(|w| w[0] == "--ro-bind"
                && w[1] == copy.to_string_lossy()
                && w[2] == place.to_string_lossy()));
        }
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// The person's ssh config is copied with `ControlMaster no` before
    /// it, kept in step with it, and bound in its place; one kept
    /// unreadable is left alone.
    #[test]
    fn the_ssh_config_is_shown_from_a_copy_that_makes_no_control_socket() {
        const FIRST: &str = agent_client::SSH_CONFIG_FIRST;
        let base =
            std::env::temp_dir().join(format!("agent-sandbox-ssh-copy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("home/.ssh")).unwrap();
        // By its real path, as the copy is named: macOS's temp is a link.
        let base = std::fs::canonicalize(&base).unwrap();
        let file = base.join("home/.ssh/config");
        let copy = base.join("copies/home/.ssh/config");
        std::fs::write(&file, "Host *\n  ControlMaster auto\n").unwrap();
        assert!(copied_file(&file, FIRST, &copy));
        let read = || std::fs::read_to_string(&copy).unwrap();
        assert_eq!(read(), format!("{FIRST}Host *\n  ControlMaster auto\n"));
        // First of all, so it stands for every host whatever the config says.
        let first = (read().lines())
            .find(|line| !line.starts_with('#') && !line.is_empty())
            .map(str::to_owned);
        assert_eq!(first.as_deref(), Some("ControlMaster no"));
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&copy).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // Edited: copied again. Taken away: made again.
        std::fs::write(&file, "Host box\n  User me\n").unwrap();
        assert!(copied_file(&file, FIRST, &copy));
        assert_eq!(read(), format!("{FIRST}Host box\n  User me\n"));
        std::fs::remove_file(&copy).unwrap();
        assert!(copied_file(&file, FIRST, &copy));
        assert!(read().ends_with("Host box\n  User me\n"));
        // None there, none copied.
        assert!(!copied_file(
            &base.join("gone"),
            FIRST,
            &base.join("copies/gone")
        ));
        // Shown by its real path, a link followed, as bubblewrap binds,
        // its copy kept under that path.
        let copies = base.join("copies");
        let keyed = PathBuf::from(format!(
            "{}.{:016x}",
            copies.join(file.strip_prefix("/").unwrap()).display(),
            fnv(FIRST.as_bytes())
        ));
        let link = base.join("home/.ssh/linked");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        let shown = copied_in(&copies, &link, FIRST, &[]).unwrap();
        assert_eq!(shown, (keyed.clone(), file.clone()));
        assert!(std::fs::read_to_string(&keyed).unwrap().starts_with(FIRST));
        // Kept unreadable, by the folder it is in: not shown, and the copy
        // made before, which sandboxes could read under ~/.agent, removed.
        let hidden = [base.join("home/.ssh")];
        assert_eq!(copied_in(&copies, &link, FIRST, &hidden), None);
        assert!(!keyed.exists());
        assert!(copied_in(&copies, &file, FIRST, &[]).is_some());
        assert_eq!(copied_in(&copies, &file, FIRST, &hidden), None);
        assert!(!keyed.exists());
        // Other text first, as another daemon may put: a copy of its own.
        let (mine, other) = (
            copied_in(&copies, &file, FIRST, &[]).unwrap(),
            copied_in(&copies, &file, "Other yes\n", &[]).unwrap(),
        );
        assert_ne!(mine.0, other.0);
        assert!(std::fs::read_to_string(&mine.0).unwrap().starts_with(FIRST));
        assert!(
            std::fs::read_to_string(&other.0)
                .unwrap()
                .starts_with("Other yes\n")
        );
        assert!(copied_file(&file, FIRST, &copy));
        #[cfg(target_os = "linux")]
        {
            let policy = Policy {
                owned: vec![(copy.clone(), file.clone())],
                ..Policy::new(Some(Mode::ReadOnly), None, &[], false, &Daemon::default())
                    .unwrap()
                    .unwrap()
            };
            let args = linux::args(&policy, false);
            assert!(args.windows(3).any(|w| w[0] == "--ro-bind"
                && w[1] == copy.to_string_lossy()
                && w[2] == file.to_string_lossy()));
        }
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// The crypto-policies back-ends, links into the active policy's own
    /// files elsewhere, are copied as the files they point at, and copied
    /// again when the person switches policy, which points them anew.
    #[test]
    fn a_folder_of_links_is_copied_as_the_files_they_point_at() {
        let base = std::env::temp_dir().join(format!("agent-sandbox-links-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let place = base.join("etc/crypto-policies/back-ends");
        let copy = base.join("copies/etc/crypto-policies/back-ends");
        let policies = base.join("usr/share/crypto-policies");
        for (policy, ciphers) in [("DEFAULT", "aes256-gcm"), ("FUTURE", "chacha20")] {
            std::fs::create_dir_all(policies.join(policy)).unwrap();
            std::fs::write(
                policies.join(policy).join("openssh.txt"),
                format!("Ciphers {ciphers}\n"),
            )
            .unwrap();
        }
        std::fs::create_dir_all(&place).unwrap();
        let point = |policy: &str| {
            let _ = std::fs::remove_file(place.join("openssh.config"));
            std::os::unix::fs::symlink(
                policies.join(policy).join("openssh.txt"),
                place.join("openssh.config"),
            )
            .unwrap();
        };
        point("DEFAULT");
        // A link to nothing is left out, as a file it can't read.
        std::os::unix::fs::symlink(base.join("gone"), place.join("nss.config")).unwrap();
        assert!(copied(&place, &copy));
        let config = copy.join("openssh.config");
        assert!(std::fs::symlink_metadata(&config).unwrap().is_file());
        let read = || std::fs::read_to_string(&config).unwrap();
        assert_eq!(read(), "Ciphers aes256-gcm\n");
        assert!(!copy.join("nss.config").exists());
        // `update-crypto-policies --set FUTURE` points the link elsewhere.
        point("FUTURE");
        assert!(copied(&place, &copy));
        assert_eq!(read(), "Ciphers chacha20\n");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_git_folder_given_as_a_root_keeps_what_git_runs() {
        let base = std::env::temp_dir().join(format!("agent-sandbox-git-{}", std::process::id()));
        let git = base.join(".git");
        std::fs::create_dir_all(git.join("objects")).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(git.join("config"), "").unwrap();
        std::fs::create_dir_all(git.join("worktrees/wt")).unwrap();
        std::fs::write(git.join("worktrees/wt/commondir"), "../..\n").unwrap();
        let base = std::fs::canonicalize(&base).unwrap();
        let git = base.join(".git");
        let policy = Policy::new(
            Some(Mode::WorkspaceWrite),
            Some(&base),
            &[git.to_str().unwrap().to_owned()],
            false,
            &protecting(),
        )
        .unwrap()
        .unwrap();
        assert!(policy.can_write(&git.join("objects/ab/cdef")));
        assert!(policy.can_write(&git.join("index.lock")));
        assert!(!policy.can_write(&git.join("config")));
        // Missing hooks may not be made, nor a submodule's git folder.
        assert!(!policy.can_write(&git.join("hooks/pre-commit")));
        assert!(!policy.can_write(&git.join("modules/sub/hooks/pre-commit")));
        // Nor a `commondir`, which would have git take another folder's hooks
        // and config, here or in a linked worktree's folder.
        assert!(!policy.can_write(&git.join("commondir")));
        assert!(!policy.can_write(&git.join("worktrees/wt/commondir")));
        // A linked worktree's folder is its own bot's to write, on Linux
        // none other's: this bot works in the main checkout.
        assert_eq!(
            policy.can_write(&git.join("worktrees/wt/index.lock")),
            !cfg!(target_os = "linux")
        );
        // A link where git keeps what it runs is that place's to keep,
        // wherever it points: it may not be removed.
        std::fs::create_dir_all(git.join("hooks")).unwrap();
        std::fs::write(base.join("hook.sh"), "").unwrap();
        std::os::unix::fs::symlink("../../hook.sh", git.join("hooks/pre-commit")).unwrap();
        assert!(policy.can_write(&git.join("hooks/pre-commit")));
        assert!(!policy.can_remove(&git.join("hooks/pre-commit")));
        assert!(policy.can_remove(&base.join("hook.sh")));
        // With per-worktree config on, whose files git reads wherever they
        // are made, none of it is written.
        std::fs::write(
            git.join("config"),
            "[extensions]\n\tworktreeConfig = true\n",
        )
        .unwrap();
        let policy = Policy::new(
            Some(Mode::WorkspaceWrite),
            Some(&base),
            &[git.to_str().unwrap().to_owned()],
            false,
            &protecting(),
        )
        .unwrap()
        .unwrap();
        assert!(!policy.can_write(&git.join("objects/ab/cdef")));
        assert!(!policy.can_write(&git.join("worktrees/x/config.worktree")));
        // As with config it includes, which may be anywhere.
        std::fs::write(git.join("config"), "[include]\n\tpath = extra.conf\n").unwrap();
        let policy = Policy::new(
            Some(Mode::WorkspaceWrite),
            Some(&base),
            &[git.to_str().unwrap().to_owned()],
            false,
            &protecting(),
        )
        .unwrap()
        .unwrap();
        assert!(!policy.can_write(&git.join("extra.conf")));
        // Nothing in it is held either: no placeholder is made inside it.
        let kept = policy.roots.iter().find(|root| root.path == git).unwrap();
        assert_eq!(kept.read_only, std::slice::from_ref(&git));
        assert!(kept.missing.is_empty() && kept.locks.is_empty());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn the_deepest_root_decides_and_seatbelt_keeps_what_it_keeps() {
        let base = std::env::temp_dir().join(format!("agent-sandbox-unit-{}", std::process::id()));
        let inner = base.join("inner");
        std::fs::create_dir_all(inner.join(".git")).unwrap();
        std::fs::create_dir_all(base.join(".git")).unwrap();
        let base = std::fs::canonicalize(&base).unwrap();
        let inner = base.join("inner");
        let policy = Policy::new(
            Some(Mode::WorkspaceWrite),
            Some(&base),
            &[inner.to_str().unwrap().to_owned()],
            false,
            &protecting(),
        )
        .unwrap()
        .unwrap();
        assert!(policy.can_write(&base.join("a")));
        assert!(policy.can_write(&inner.join("new/file")));
        assert!(!policy.can_write(&base.join(".git/config")));
        assert!(!policy.can_write(&inner.join(".git/HEAD")));
        assert!(!policy.can_write(Path::new("/etc/hosts")));
        assert!(!policy.can_write(&base.join("../../etc/x")));
        // A missing `.agents` may not be made.
        assert!(!policy.can_write(&inner.join(".agents/skills")));
        let (profile, params) = seatbelt(&policy);
        assert!(profile.contains("(allow file-read*)"));
        assert!(!profile.contains("(allow network-outbound)"));
        let key = |path: &Path| {
            params
                .iter()
                .find(|(_, named)| named == path)
                .map(|(key, _)| key.clone())
                .unwrap()
        };
        // The outer root leaves the inner root to its own rule, which keeps
        // the inner `.git`.
        let (outer, inner_key, inner_git) = (key(&base), key(&inner), key(&inner.join(".git")));
        let rule = |root: &str| {
            profile
                .split(&format!("(require-all (subpath (param \"{root}\"))"))
                .nth(1)
                .unwrap()
                .split(" )")
                .next()
                .unwrap()
                .to_owned()
        };
        assert!(rule(&outer).contains(&format!("(require-not (subpath (param \"{inner_key}\")))")));
        assert!(!rule(&outer).contains(&format!("\"{inner_git}\"")));
        assert!(
            rule(&inner_key).contains(&format!("(require-not (subpath (param \"{inner_git}\")))"))
        );
        // Paths go in as parameters, never into the profile's text.
        assert!(!profile.contains(base.to_str().unwrap()));
        // The daemon's socket is denied, network on or off; inside a root
        // it stays read-only and the folders above it stay put, so it can't
        // be renamed or linked past its rule.
        let socket = inner.join("run/agent.sock");
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        std::fs::write(&socket, "").unwrap();
        let store = inner.join("state/state.sqlite");
        std::fs::create_dir_all(store.parent().unwrap()).unwrap();
        std::fs::write(&store, "").unwrap();
        let daemon = Daemon {
            socket: Some(socket.clone()),
            store: Some(store),
            ..protecting()
        };
        for network in [false, true] {
            let policy = Policy::new(
                Some(Mode::WorkspaceWrite),
                Some(&base),
                &[inner.to_str().unwrap().to_owned()],
                network,
                &daemon,
            )
            .unwrap()
            .unwrap();
            assert!(!policy.can_write(&socket));
            let (profile, params) = seatbelt(&policy);
            let key = |path: &Path| {
                params
                    .iter()
                    .find(|(_, named)| named == path)
                    .map(|(key, _)| key.clone())
                    .unwrap()
            };
            assert!(profile.contains(&format!(
                "(deny network-outbound (remote unix-socket (subpath (param \"{}\"))))",
                key(&socket)
            )));
            assert!(profile.contains(&format!(
                "(require-not (literal (param \"{}\")))",
                key(&socket)
            )));
            assert!(profile.contains(&anchor(&key(&inner.join("run")))));
            // So does the folder above a protected store.
            assert!(profile.contains(&anchor(&key(&inner.join("state")))));
            #[cfg(target_os = "linux")]
            assert!(
                linux::args(&policy, false)
                    .windows(3)
                    .any(|w| w[0] == "--ro-bind"
                        && w[1] == "/dev/null"
                        && w[2] == socket.to_string_lossy())
            );
            // With the bot's own socket, that one takes the daemon's place.
            let own = Policy {
                capability: Some(base.join("own.sock")),
                ..policy.clone()
            };
            let (profile, params) = seatbelt(&own);
            let own_key = params
                .iter()
                .find(|(_, path)| path == &base.join("own.sock"))
                .map(|(key, _)| key.clone())
                .unwrap();
            assert!(profile.contains(&format!(
                "(allow network-outbound (remote unix-socket (subpath (param \"{own_key}\"))))"
            )));
            #[cfg(target_os = "linux")]
            assert!(linux::args(&own, false).windows(3).any(|w| w[0] == "--bind"
                && w[1] == base.join("own.sock").to_string_lossy()
                && w[2] == socket.to_string_lossy()));
            // Never another bot's, from the same folder.
            let folder_key = params
                .iter()
                .find(|(_, path)| path == &base)
                .map(|(key, _)| key.clone())
                .unwrap();
            let denied = format!(
                "(deny network-outbound (remote unix-socket (subpath (param \"{folder_key}\"))))"
            );
            let allowed = format!(
                "(allow network-outbound (remote unix-socket (subpath (param \"{own_key}\"))))"
            );
            assert!(profile.find(&denied).unwrap() < profile.find(&allowed).unwrap());
            #[cfg(target_os = "linux")]
            assert!(
                linux::args(&own, false)
                    .windows(2)
                    .any(|w| w[0] == "--tmpfs" && w[1] == base.to_string_lossy())
            );
            assert_eq!(
                own.reach(),
                Some(if cfg!(target_os = "linux") {
                    socket.as_path()
                } else {
                    own.capability.as_deref().unwrap()
                })
            );
        }
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn unix_sockets_are_all_reached_with_the_network_on_and_as_named_off() {
        let base =
            std::env::temp_dir().join(format!("agent-sandbox-sockets-{}", std::process::id()));
        std::fs::create_dir_all(base.join("cache")).unwrap();
        let base = std::fs::canonicalize(&base).unwrap();
        let cache = base.join("cache");
        let policy = |network| {
            Policy::new(
                Some(Mode::WorkspaceWrite),
                Some(&base),
                &[cache.to_str().unwrap().to_owned()],
                network,
                &Daemon {
                    socket: Some(base.join("agent.sock")),
                    ..protecting()
                },
            )
            .unwrap()
            .unwrap()
        };
        assert!(policy(true).sockets.is_empty());
        let named = |network| policy(network).with_sockets(&["/tmp/ssh-agent.sock".to_owned()]);
        assert_eq!(named(true).sockets, [PathBuf::from("/tmp/ssh-agent.sock")]);
        let key = |params: &[(String, PathBuf)], path: &Path| {
            params
                .iter()
                .find(|(_, p)| p == path)
                .map(|(k, _)| k.clone())
                .unwrap()
        };
        // The network on, every socket, as Codex's full network: MCPX's,
        // Docker's, ssh-agent's, and those it listens on itself.
        let (profile, params) = seatbelt(&named(true));
        assert!(!profile.contains("(deny network-outbound (remote unix-socket))"));
        assert!(profile.contains("(allow network-bind (local unix-socket))"));
        assert!(profile.contains("(allow system-socket (socket-domain AF_UNIX))"));
        // But never the daemon's, by its name or a hard link to it.
        let daemon = key(&params, &base.join("agent.sock"));
        let denied = format!(
            "(deny network-outbound (remote unix-socket (subpath (param \"{daemon}\"))))\n\
             (deny file-link (subpath (param \"{daemon}\")))"
        );
        assert!(
            profile.find(&denied).unwrap() > profile.find("(allow network-outbound)\n").unwrap()
        );
        // The network off, only those it names.
        let (profile, params) = seatbelt(&named(false));
        let allowed = format!(
            "(allow network-outbound (remote unix-socket (subpath (param \"{}\"))))",
            key(&params, Path::new("/tmp/ssh-agent.sock"))
        );
        assert!(profile.contains(&allowed), "{allowed}");
        assert!(!profile.contains("(allow network-bind (local unix-socket))"));
        for path in [&base, &cache] {
            let own = format!(
                "(remote unix-socket (subpath (param \"{}\")))",
                key(&params, path)
            );
            assert!(!profile.contains(&own), "{own}");
        }
        // Off and naming none, none.
        let (profile, _) = seatbelt(&policy(false));
        assert!(!profile.contains("(local unix-socket"));
        assert!(!profile.contains("(allow system-socket (socket-domain AF_UNIX))"));
        // Read-only, it reaches only those it names.
        let read_only = Policy::new(Some(Mode::ReadOnly), Some(&base), &[], false, &protecting())
            .unwrap()
            .unwrap();
        assert!(read_only.sockets.is_empty());
        assert!(!seatbelt(&read_only).0.contains("(local unix-socket"));
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn a_walled_folder_s_sockets_are_never_reached_even_named_or_with_the_network_on() {
        let base =
            std::env::temp_dir().join(format!("agent-sandbox-walled-{}", std::process::id()));
        std::fs::create_dir_all(base.join("run")).unwrap();
        let base = std::fs::canonicalize(&base).unwrap();
        let run = base.join("run");
        for network in [true, false] {
            let policy = Policy::new(
                Some(Mode::WorkspaceWrite),
                Some(&base),
                &[],
                network,
                &Daemon {
                    walled: vec![run.clone()],
                    ..protecting()
                },
            )
            .unwrap()
            .unwrap()
            // Named in its settings, too.
            .with_sockets(&[run.join("host.sock").to_string_lossy().into_owned()]);
            assert_eq!(policy.walled, std::slice::from_ref(&run));
            let (profile, params) = seatbelt(&policy);
            let key = (params.iter())
                .find(|(_, path)| path == &run)
                .map(|(key, _)| key.clone())
                .unwrap();
            // After all that lets it out, by its name or a hard link to it.
            let denied = format!(
                "(deny network-outbound (remote unix-socket (subpath (param \"{key}\"))))\n\
                 (deny file-link (subpath (param \"{key}\")))"
            );
            let at = profile.find(&denied).unwrap();
            if network {
                assert!(at > profile.find("(allow network-outbound)\n").unwrap());
            }
            assert!(
                at > profile
                    .find("(allow network-outbound (remote unix-socket")
                    .unwrap_or(0)
            );
            // Nor renamed out from under its rule, or written, inside a root.
            assert!(policy.can_write(&base));
            assert!(!policy.can_write(&run.join("host.sock")));
            #[cfg(target_os = "linux")]
            assert!(linux::args(&policy, false).windows(6).any(|w| w
                == [
                    "--perms",
                    "555",
                    "--tmpfs",
                    &run.to_string_lossy(),
                    "--remount-ro",
                    &run.to_string_lossy()
                ]));
        }
        // A wall another program removed is made again, private, and
        // hidden all the same: bubblewrap could not make its mount point on
        // the read-only root, and a command without the wall would reach
        // the sockets of one made again while it runs.
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::PermissionsExt;
            let policy = Policy::new(
                Some(Mode::WorkspaceWrite),
                Some(&base),
                &[],
                true,
                &Daemon {
                    walled: vec![run.clone()],
                    ..protecting()
                },
            )
            .unwrap()
            .unwrap();
            let hidden = |policy: &Policy| {
                linux::args(policy, false)
                    .windows(2)
                    .any(|w| w == ["--tmpfs", &run.to_string_lossy()])
            };
            std::fs::remove_dir(&run).unwrap();
            assert!(hidden(&policy));
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(hold(&policy))
                .release();
            let made = std::fs::metadata(&run).unwrap();
            assert!(made.is_dir());
            assert_eq!(made.permissions().mode() & 0o777, 0o700);
        }
        // A root inside the wall is refused: its sockets could be renamed
        // out to a folder the rule doesn't name.
        std::fs::create_dir_all(run.join("session")).unwrap();
        for cwd in [run.clone(), run.join("session")] {
            let error = Policy::new(
                Some(Mode::WorkspaceWrite),
                Some(&cwd),
                &[],
                true,
                &Daemon {
                    walled: vec![run.clone()],
                    ..protecting()
                },
            )
            .unwrap_err();
            assert_eq!(error.code, "sandbox_unavailable");
        }
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn many_nested_roots_still_fit_one_invocation() {
        // 64 nested roots with long names, about the settings' limit: each
        // rule names only its own, so the profile and its parameters grow
        // with the roots, not with their square.
        let mut path = PathBuf::from("/");
        let roots: Vec<Root> = (0..64)
            .map(|index| {
                path.push(format!("{index:0>12}"));
                Root {
                    read_only: [".git", ".agents", ".codex", ".aws"]
                        .iter()
                        .map(|name| path.join(name))
                        .collect(),
                    missing: Vec::new(),
                    left: Vec::new(),
                    locks: Vec::new(),
                    others: Vec::new(),
                    opened: Vec::new(),
                    path: path.clone(),
                }
            })
            .collect();
        let policy = Policy {
            roots,
            network: true,
            socket: Some(path.join("agent.sock")),
            capability: None,
            listening: None,
            helper: None,
            sockets: Vec::new(),
            walled: Vec::new(),
            owned: Vec::new(),
            daemons: Vec::new(),
            unreadable: Vec::new(),
            domains: BTreeMap::new(),
        };
        let (profile, params) = seatbelt(&policy);
        let size = profile.len()
            + params
                .iter()
                .map(|(key, path)| key.len() + path.as_os_str().len() + 4)
                .sum::<usize>();
        assert!(size < 1 << 20, "{size}");
        assert_eq!(params.len(), 64 * 5 + 1);
    }

    #[test]
    fn a_symlinked_git_folder_refuses_the_sandbox() {
        let base = std::env::temp_dir().join(format!("agent-sandbox-link-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        std::os::unix::fs::symlink("/", base.join(".git")).unwrap();
        let error = Policy::new(
            Some(Mode::WorkspaceWrite),
            Some(&base),
            &[],
            false,
            &protecting(),
        )
        .unwrap_err();
        assert_eq!(error.code, "sandbox_unavailable");
        std::fs::remove_dir_all(base).unwrap();
    }

    /// A folder named since but gone is skipped, as Codex skips it, rather
    /// than refusing every command; an empty protected folder is taken as
    /// missing, mounted over afresh, never bound from the disk.
    #[test]
    fn a_gone_root_is_skipped_and_an_empty_placeholder_is_missing() {
        let base = std::env::temp_dir().join(format!("agent-sandbox-gone-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join(".agents")).unwrap();
        std::fs::create_dir_all(base.join(".codex/skills")).unwrap();
        let base = std::fs::canonicalize(&base).unwrap();
        let gone = base.join("gone/.git");
        let policy = Policy::new(
            Some(Mode::WorkspaceWrite),
            Some(&base),
            &[gone.to_str().unwrap().to_owned()],
            false,
            &protecting(),
        )
        .unwrap()
        .unwrap();
        assert!(policy.roots.iter().all(|root| root.path != gone));
        let root = policy.roots.iter().find(|root| root.path == base).unwrap();
        assert!(root.missing.contains(&base.join(".agents")));
        assert!(!root.read_only.contains(&base.join(".agents")));
        // One with something in it is the person's, bound as it is.
        assert!(root.read_only.contains(&base.join(".codex")));
        assert!(!policy.can_write(&base.join(".agents/x")));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A placeholder stays while any sandbox holds it and goes with the
    /// last; one a crash left is removed by the next to hold it; an empty
    /// folder this sandbox didn't make is left, as Codex leaves it.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn the_last_holder_removes_only_the_placeholder_it_made() {
        let base = std::env::temp_dir().join(format!("agent-sandbox-held-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join(".codex")).unwrap();
        let base = std::fs::canonicalize(&base).unwrap();
        let policy = Policy::new(
            Some(Mode::WorkspaceWrite),
            Some(&base),
            &[],
            false,
            &protecting(),
        )
        .unwrap()
        .unwrap();
        let (git, codex) = (base.join(".git"), base.join(".codex"));
        let first = hold(&policy).await;
        assert!(git.is_dir(), "made before the sandbox is");
        let second = hold(&policy).await;
        first.release();
        assert!(git.is_dir(), "still held by the second");
        second.release();
        assert!(!git.exists(), "the last one out removes it");
        assert!(codex.is_dir(), "an empty folder it didn't make stays");
        // A crash: the locks go with the process, the placeholder stays.
        let mut crashed = hold(&policy).await;
        drop(std::mem::take(&mut crashed.0));
        drop(crashed);
        assert!(git.is_dir());
        hold(&policy).await.release();
        assert!(!git.exists(), "the next holder removes what a crash left");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A hold over many sandboxes keeps the pipes of only those still
    /// running, however many it waited for.
    #[test]
    fn a_hold_keeps_only_the_ends_of_sandboxes_still_running() {
        let mut held = Hold(Vec::new(), Vec::new(), None, Vec::new());
        let (running, _open) = std::io::pipe().unwrap();
        held.waits_for(Ended(Some(running.into())));
        for _ in 0..100 {
            let (ended, closed) = std::io::pipe().unwrap();
            drop(closed);
            held.waits_for(Ended(Some(ended.into())));
        }
        assert_eq!(held.3.len(), 2, "the running one and the last one asked");
    }

    /// Two holders letting go at the same moment: one of them removes the
    /// placeholder, never neither.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn holders_letting_go_at_once_leave_no_placeholder() {
        let base = std::env::temp_dir().join(format!("agent-sandbox-once-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let base = std::fs::canonicalize(&base).unwrap();
        let policy = Policy::new(
            Some(Mode::WorkspaceWrite),
            Some(&base),
            &[],
            false,
            &protecting(),
        )
        .unwrap()
        .unwrap();
        let git = base.join(".git");
        for _ in 0..200 {
            let holds = [hold(&policy).await, hold(&policy).await];
            assert!(git.is_dir());
            let at_once = std::sync::Arc::new(std::sync::Barrier::new(2));
            let going: Vec<_> = (holds.into_iter())
                .map(|held| {
                    let at_once = at_once.clone();
                    std::thread::spawn(move || {
                        at_once.wait();
                        held.release();
                    })
                })
                .collect();
            going.into_iter().for_each(|going| going.join().unwrap());
            assert!(!git.exists(), "one of them removes it");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A bot working in a linked worktree, given its repository's git
    /// folder to commit to, writes its own worktree's folder there and no
    /// other's, and can't rename one to put a `commondir` of its own in its
    /// place: the person's git in that worktree would run what it names.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_worktree_writes_its_own_folder_in_the_git_folder_and_no_other() {
        let base = std::env::temp_dir().join(format!("agent-sandbox-wt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let git = base.join("repo/.git");
        std::fs::create_dir_all(git.join("objects")).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(git.join("config"), "").unwrap();
        for name in ["mine", "theirs"] {
            std::fs::create_dir_all(git.join("worktrees").join(name)).unwrap();
            std::fs::write(
                git.join("worktrees").join(name).join("commondir"),
                "../..\n",
            )
            .unwrap();
            let checkout = base.join(name);
            std::fs::create_dir_all(&checkout).unwrap();
            std::fs::write(
                checkout.join(".git"),
                format!("gitdir: {}\n", git.join("worktrees").join(name).display()),
            )
            .unwrap();
        }
        let base = std::fs::canonicalize(&base).unwrap();
        let git = base.join("repo/.git");
        let mine = base.join("mine");
        let policy = Policy::new(
            Some(Mode::WorkspaceWrite),
            Some(&mine),
            &[git.to_str().unwrap().to_owned()],
            false,
            &protecting(),
        )
        .unwrap()
        .unwrap();
        assert!(policy.can_write(&git.join("objects/ab/cdef")));
        assert!(policy.can_write(&git.join("worktrees/mine/index.lock")));
        assert!(!policy.can_write(&git.join("worktrees/mine/commondir")));
        assert!(!policy.can_write(&git.join("worktrees/theirs/index.lock")));
        assert!(!policy.can_write(&git.join("worktrees/theirs/commondir")));
        assert!(!policy.can_write(&git.join("worktrees/new/commondir")));
        // A folder inside the checkout writes its checkout's own too.
        std::fs::create_dir_all(mine.join("src")).unwrap();
        let nested = Policy::new(
            Some(Mode::WorkspaceWrite),
            Some(&mine.join("src")),
            &[git.to_str().unwrap().to_owned()],
            false,
            &protecting(),
        )
        .unwrap()
        .unwrap();
        assert!(nested.can_write(&git.join("worktrees/mine/index.lock")));
        assert!(!nested.can_write(&git.join("worktrees/theirs/index.lock")));
        if available().is_ok() {
            let worktrees = git.join("worktrees");
            let script = "cd \"$1\" && { mv theirs gone || true; } && { mv mine gone || true; } \
                && { echo x > mine/commondir || true; } && echo ok > mine/HEAD";
            let folder = worktrees.to_str().unwrap();
            let (mut sandboxed, ended) =
                command(&policy, "sh", &["-c", script, "sh", folder], false).unwrap();
            let mut held = hold(&policy).await;
            let status = sandboxed.current_dir(&mine).status().await.unwrap();
            drop(sandboxed);
            held.waits_for(ended);
            held.release();
            assert!(status.success(), "its own worktree's HEAD is its to write");
            assert!(
                worktrees.join("theirs/commondir").exists(),
                "no other is renamed"
            );
            assert!(!worktrees.join("gone").exists());
            assert_eq!(
                std::fs::read_to_string(worktrees.join("mine/commondir")).unwrap(),
                "../..\n"
            );
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A command that leaves something running: bubblewrap's init, which
    /// holds the placeholder too, outlives it until the sandbox is taken
    /// down. Its hold lets go once the sandbox is gone, so the placeholder
    /// is removed rather than left with nothing holding it.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_hold_lets_go_once_its_sandbox_is_gone() {
        if available().is_err() {
            return;
        }
        let base = std::env::temp_dir().join(format!("agent-sandbox-gone-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let base = std::fs::canonicalize(&base).unwrap();
        let policy = Policy::new(
            Some(Mode::WorkspaceWrite),
            Some(&base),
            &[],
            false,
            &protecting(),
        )
        .unwrap()
        .unwrap();
        let git = base.join(".git");
        for _ in 0..20 {
            let mut held = hold(&policy).await;
            let (mut sandboxed, ended) =
                command(&policy, "sh", &["-c", "sleep 5 >/dev/null 2>&1 &"], false).unwrap();
            let status = sandboxed.current_dir(&base).status().await.unwrap();
            assert!(status.success());
            drop(sandboxed);
            held.waits_for(ended);
            held.release();
            assert!(!git.exists(), "removed once the sandbox is gone");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A git push over ssh, in a sandbox with the network on, through a
    /// real sshd, as the person's config says `ControlMaster auto` with
    /// its `ControlPath` in a folder the sandbox keeps read-only: ssh can't
    /// make its socket there and fails the push, until the sandbox shows
    /// the config from its copy, when the push and a plain ssh both go
    /// through, and join a master the person has open. Needs sshd, ssh and
    /// git; AGENT_TEST_SSH makes one missing a failure.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_push_over_ssh_goes_through_with_a_control_master_config() {
        let needed = [
            "/usr/sbin/sshd",
            "/usr/bin/ssh",
            "/usr/bin/ssh-keygen",
            "/usr/bin/git",
        ];
        let missing: Vec<_> = needed
            .iter()
            .filter(|program| !Path::new(program).is_file())
            .collect();
        if !missing.is_empty() || available().is_err() {
            assert!(
                std::env::var_os("AGENT_TEST_SSH").is_none(),
                "missing {missing:?} or no sandbox: {:?}",
                available()
            );
            return;
        }
        let base = std::env::temp_dir().join(format!("agent-sandbox-ssh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("ssh")).unwrap();
        let base = std::fs::canonicalize(&base).unwrap();
        let ssh = base.join("ssh");
        let run = |program: &str, args: &[&str]| {
            let output = std::process::Command::new(program)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{program} {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).into_owned()
        };
        let at = |name: &str| ssh.join(name).to_string_lossy().into_owned();
        for key in ["host", "id"] {
            run(
                "/usr/bin/ssh-keygen",
                &["-q", "-t", "ed25519", "-N", "", "-f", &at(key)],
            );
        }
        std::fs::copy(ssh.join("id.pub"), ssh.join("authorized_keys")).unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        std::fs::write(
            ssh.join("sshd_config"),
            format!(
                "Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nAuthorizedKeysFile {}\n\
                 PasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\n\
                 StrictModes no\nPidFile none\nPermitRootLogin prohibit-password\n",
                at("host"),
                at("authorized_keys")
            ),
        )
        .unwrap();
        // The person's config, its control socket beside it, as on the
        // machine that found this.
        std::fs::write(
            ssh.join("config"),
            format!(
                "Host *\n  ControlMaster auto\n  ControlPath {}/control-%r@%h:%p\n  \
                 ControlPersist 1m\n  IdentityFile {}\n  IdentitiesOnly yes\n  \
                 StrictHostKeyChecking no\n  UserKnownHostsFile /dev/null\n  LogLevel ERROR\n",
                ssh.display(),
                at("id")
            ),
        )
        .unwrap();
        let mut sshd = std::process::Command::new("/usr/sbin/sshd")
            .args(["-D", "-e", "-f", &at("sshd_config")])
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let ssh_command = format!("ssh -F {} -p {port}", at("config"));
        let remote = base.join("remote.git");
        let work = base.join("work");
        run(
            "/usr/bin/git",
            &["init", "-q", "--bare", &remote.to_string_lossy()],
        );
        run("/usr/bin/git", &["init", "-q", &work.to_string_lossy()]);
        let git = |args: &[&str]| {
            let mut all = vec!["-C", work.to_str().unwrap()];
            all.extend(args);
            run("/usr/bin/git", &all)
        };
        git(&[
            "-c",
            "user.name=a",
            "-c",
            "user.email=a@a",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "one",
        ]);
        git(&[
            "remote",
            "add",
            "origin",
            &format!("ssh://127.0.0.1{}", remote.display()),
        ]);
        // sshd takes a moment to listen.
        for _ in 0..50 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        // The agent's folder is the checkout; the person's ssh folder stays
        // read-only, as their home does, though it is under a temporary
        // folder a sandbox may write.
        let daemon = Daemon {
            protected_paths: vec![ssh.clone()],
            ..protecting()
        };
        let mut policy = Policy::new(Some(Mode::WorkspaceWrite), Some(&work), &[], true, &daemon)
            .unwrap()
            .unwrap();
        // Run as the daemon runs one: its placeholders held first, so no
        // other test's sandbox, letting the same ones go, holds their
        // lock to remove them while bubblewrap's init takes its own.
        async fn sandboxed(
            policy: &Policy,
            work: &Path,
            ssh_command: &str,
            program: &str,
            args: &[&str],
        ) -> std::io::Result<std::process::Output> {
            let (mut command, ended) = command(policy, program, args, false).unwrap();
            let mut held = hold(policy).await;
            let output = command
                .current_dir(work)
                .env("GIT_SSH_COMMAND", ssh_command)
                .output()
                .await;
            drop(command);
            held.waits_for(ended);
            held.release();
            output
        }
        let push = ["push", "-q", "origin", "HEAD:main"];
        // As it was: ssh, with no master open, can't make its socket.
        let refused = sandboxed(&policy, &work, &ssh_command, "git", &push)
            .await
            .unwrap();
        let said = String::from_utf8_lossy(&refused.stderr).into_owned();
        assert!(!refused.status.success(), "pushed without the copy");
        assert!(said.contains("Read-only file system"), "{said}");
        // Shown from the person's copy, which makes no control socket.
        let copy = base.join("copies/ssh/config");
        assert!(copied_file(
            &ssh.join("config"),
            agent_client::SSH_CONFIG_FIRST,
            &copy
        ));
        policy.owned.push((copy, ssh.join("config")));
        let pushed = sandboxed(&policy, &work, &ssh_command, "git", &push)
            .await
            .unwrap();
        assert!(
            pushed.status.success(),
            "{}",
            String::from_utf8_lossy(&pushed.stderr)
        );
        let head = git(&["rev-parse", "HEAD"]);
        let main = run(
            "/usr/bin/git",
            &["-C", &remote.to_string_lossy(), "rev-parse", "main"],
        );
        assert_eq!(head, main);
        let plain = [
            "-F",
            &at("config"),
            "-p",
            &port.to_string(),
            "127.0.0.1",
            "echo",
            "reached",
        ];
        let reached = sandboxed(&policy, &work, &ssh_command, "ssh", &plain)
            .await
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&reached.stdout), "reached\n");
        // A master the person opened, outside any sandbox, is joined.
        run(
            "/usr/bin/ssh",
            &[
                "-F",
                &at("config"),
                "-p",
                &port.to_string(),
                "-fN",
                "127.0.0.1",
            ],
        );
        let mut verbose = vec!["-v"];
        verbose.extend(plain);
        let joined = sandboxed(&policy, &work, &ssh_command, "ssh", &verbose)
            .await
            .unwrap();
        let said = String::from_utf8_lossy(&joined.stderr).into_owned();
        assert!(joined.status.success(), "{said}");
        assert!(said.contains("mux_client_request_session"), "{said}");
        let _ = std::process::Command::new("/usr/bin/ssh")
            .args([
                "-F",
                &at("config"),
                "-p",
                &port.to_string(),
                "-O",
                "exit",
                "127.0.0.1",
            ])
            .output();
        let _ = sshd.kill();
        let _ = sshd.wait();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// bubblewrap's init holds the lock while the sandbox runs, out of the
    /// command's reach: a placeholder a running sandbox mounts is never
    /// removed under it, whoever lets go first.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_running_sandbox_keeps_its_placeholder() {
        if available().is_err() {
            return;
        }
        let base = std::env::temp_dir().join(format!("agent-sandbox-run-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let base = std::fs::canonicalize(&base).unwrap();
        let policy = Policy::new(
            Some(Mode::WorkspaceWrite),
            Some(&base),
            &[],
            false,
            &protecting(),
        )
        .unwrap()
        .unwrap();
        let git = base.join(".git");
        // Built before its hold, as a detached command is: the lock it names
        // was made as the policy was.
        let (mut sandboxed, ended) =
            command(&policy, "sh", &["-c", "sleep 1; mkdir .git/x"], false).unwrap();
        let held = hold(&policy).await;
        let mut child = sandboxed.current_dir(&base).spawn().unwrap();
        drop(sandboxed);
        // The daemon lets go first, as when it restarts under a detached
        // command: the sandbox's own lock keeps the placeholder.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        held.release();
        assert!(git.is_dir());
        let status = child.wait().await.unwrap();
        assert!(!status.success(), "the placeholder stayed read-only");
        assert!(!git.join("x").exists());
        // The next hold to let go, once the sandbox is gone, removes it.
        let mut next = hold(&policy).await;
        next.waits_for(ended);
        next.release();
        assert!(!git.exists());
        let _ = std::fs::remove_dir_all(&base);
    }
}
