//! Confining a bot's commands as Codex does: its three sandbox modes, its
//! writable roots with read-only folders inside them, bubblewrap on Linux
//! and Seatbelt on macOS. The bubblewrap arguments and the Seatbelt policy
//! are ported from Codex's `linux-sandbox` and `sandboxing` crates
//! (Apache-2.0; see NOTICE), and kept in step with them.
//!
//! A bot with a sandbox writes only its roots: its folder, the folders its
//! client named, and the temporary directories. Reading stays open. Inside
//! each root, what git, agent configuration and AWS profiles run or trust
//! stays read-only, and so does the daemon's own store. Network is off
//! unless the bot's client turned it on.

use crate::{Result, fail_with};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

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

/// Folders at the top of a writable root that stay read-only, as Codex keeps
/// them: what git runs (hooks, config), what agents trust, and AWS profiles,
/// which can name a program to run for credentials. A missing one is kept
/// from being made, as Codex does for its own metadata.
const PROTECTED: [&str; 4] = [".git", ".agents", ".codex", ".aws"];
/// Of those, the ones kept from being made when missing: a new `.git`
/// would be run by git outside the sandbox, and a new `.agents` read as
/// the project's own settings.
const PROTECTED_MISSING: [&str; 3] = [".git", ".agents", ".codex"];

/// A folder the bot may write, and what inside it stays read-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    pub path: PathBuf,
    /// Existing paths inside the root that stay read-only.
    pub read_only: Vec<PathBuf>,
    /// Paths inside the root that don't exist and may not be made.
    pub missing: Vec<PathBuf>,
}

/// What a sandboxed command may do: write its roots, and reach the network
/// when `network` is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// Shallowest first: a deeper root decides for what is inside it.
    pub roots: Vec<Root>,
    pub network: bool,
    /// The daemon's socket, which no command in the sandbox may reach.
    pub socket: Option<PathBuf>,
    /// The agent binary a confined write runs in ([`WRITE`]).
    pub helper: Option<PathBuf>,
}

impl Policy {
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
        let mut roots = Vec::new();
        for path in paths {
            roots.push(root(&path, daemon)?);
        }
        Ok(Some(Policy {
            roots,
            network: network && mode == Mode::WorkspaceWrite,
            socket: daemon.socket.clone(),
            helper: daemon.helper.clone(),
        }))
    }

    /// Whether a command in this sandbox may write `path`, an absolute
    /// path: the deepest root that holds it decides, as the sandbox's
    /// mounts and rules do. Symlinks in what exists of it are followed, as
    /// a write would follow them.
    pub fn can_write(&self, path: &Path) -> bool {
        let Some(path) = real(path) else {
            return false;
        };
        self.roots
            .iter()
            .rev()
            .find(|root| path.starts_with(&root.path))
            .is_some_and(|root| {
                !root
                    .read_only
                    .iter()
                    .chain(&root.missing)
                    .any(|kept| path.starts_with(kept))
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

/// Read what the escape check reads once, at start, rather than on the
/// runtime thread when the first call comes.
pub fn warm() {
    temporary();
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

/// The confined write [`WRITE`] asks for, run inside the sandbox: make the
/// folders above `path`, then replace its content with stdin's. Only a
/// regular file is written.
pub fn write(path: &Path) -> Result<()> {
    use std::io::{Read, Write};
    let denied = |error: std::io::Error| match error.raw_os_error() {
        Some(libc::EACCES | libc::EPERM | libc::EROFS) => {
            crate::Error::with("write_denied", path.display().to_string())
        }
        _ => crate::Error::with("io_error", error.to_string()),
    };
    let mut content = Vec::new();
    std::io::stdin().read_to_end(&mut content)?;
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
    file.write_all(&content).map_err(denied)?;
    Ok(())
}

/// The tools a sandbox confines: what runs a command or writes a file.
pub fn confinable(tool: &str) -> bool {
    matches!(tool, "shell" | "write" | "edit" | "apply_patch")
}

/// Whether a call by a bot in `mode` asks to run outside its sandbox: a
/// command that asks to escalate, or a write, edit or patch of a path
/// outside every folder the bot may write. `root` is the bot's folder and `folder`
/// the turn's, against which a relative path is read. Decided from the call
/// and the folders' names alone, never from the files as they are now, so
/// the round that announces a call and the run that starts it agree; a
/// confined write is still checked against the real path when it runs.
pub fn escapes(
    mode: Mode,
    root: Option<&Path>,
    writable: &[String],
    tool: &str,
    arguments: &str,
    folder: &Path,
) -> bool {
    let Ok(arguments) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return false;
    };
    match tool {
        "shell" => arguments["sandbox_permissions"] == "require_escalated",
        "write" | "edit" => arguments["path"]
            .as_str()
            .is_some_and(|path| outside(mode, root, writable, folder, path)),
        // A patch escapes when any file it changes would.
        "apply_patch" => arguments["input"]
            .as_str()
            .and_then(|input| crate::tools::Patch::parse(input).ok())
            .is_some_and(|patch| {
                patch
                    .paths()
                    .any(|path| outside(mode, root, writable, folder, path))
            }),
        _ => false,
    }
}

/// Whether `path`, read against `folder`, lies outside every folder a bot
/// in `mode` may write.
fn outside(
    mode: Mode,
    root: Option<&Path>,
    writable: &[String],
    folder: &Path,
    path: &str,
) -> bool {
    let target = lexical(&folder.join(path));
    let inside = |dir: &Path| target.starts_with(dir);
    let temporary = temporary()
        .iter()
        .any(|(named, real)| mode == Mode::WorkspaceWrite && (inside(named) || inside(real)));
    !(temporary
        || writable_paths(mode, root, writable)
            .iter()
            .any(|dir| inside(dir)))
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
    pub socket: Option<PathBuf>,
    /// The agent binary, which a confined write runs in.
    pub helper: Option<PathBuf>,
}

/// The root at `path`, with what stays read-only inside it: the protected
/// folders at its top, a worktree's own git folder when it is inside the
/// root, and the daemon's store and socket.
fn root(path: &Path, daemon: &Daemon) -> Result<Root> {
    let mut read_only = Vec::new();
    let mut missing = Vec::new();
    for name in PROTECTED {
        let kept = path.join(name);
        if held(&kept) {
            missing.push(kept);
            continue;
        }
        match std::fs::symlink_metadata(&kept) {
            // A symlink the sandbox may replace can't be held read-only:
            // whatever is mounted over its target, the link itself can be
            // swapped for a folder of the command's own. Codex refuses it
            // too.
            Ok(found) if found.file_type().is_symlink() => {
                return fail_with(
                    "sandbox_unavailable",
                    format!(
                        "{} is a symlink in a folder the bot may write, so it can't be kept \
                         read-only",
                        kept.display()
                    ),
                );
            }
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
            Err(_) if PROTECTED_MISSING.contains(&name) => missing.push(kept),
            Err(_) => {}
        }
    }
    if let Some(store) = &daemon.store
        && store.starts_with(path)
    {
        read_only.extend(store_files(store).filter(|file| file.exists()));
    }
    // A socket the bot could rename or link would be reachable by its new
    // name, past a rule that names the old one.
    if let Some(socket) = &daemon.socket
        && socket.starts_with(path)
        && socket.exists()
    {
        read_only.push(socket.clone());
    }
    Ok(Root {
        path: path.to_owned(),
        read_only,
        missing,
    })
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

/// The folder a `.git` file names, by its real path.
fn gitdir(file: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(file).ok()?;
    let named = text.trim().strip_prefix("gitdir:")?.trim();
    std::fs::canonicalize(file.parent()?.join(named)).ok()
}

fn depth(path: &Path) -> usize {
    path.components().count()
}

/// `path` with what exists of it resolved to its real path and the rest
/// appended; `None` when the rest climbs out with `..`, which can't be
/// known without making it. A symlink whose target is missing is followed
/// to that target, as a write through it would be.
fn real(path: &Path) -> Option<PathBuf> {
    real_within(path, 40)
}

fn real_within(path: &Path, links: u32) -> Option<PathBuf> {
    let mut existing = path.to_owned();
    let mut rest = Vec::new();
    loop {
        let found = std::fs::canonicalize(&existing).ok().or_else(|| {
            // canonicalize fails on a dangling link as on a missing name.
            let link = std::fs::read_link(&existing).ok()?;
            let mut target = existing.parent()?.join(link);
            for part in rest.iter().rev() {
                target.push(part);
            }
            rest.clear();
            real_within(&target, links.checked_sub(1)?)
        });
        if let Some(mut resolved) = found {
            for part in rest.iter().rev() {
                resolved.push(part);
            }
            return Some(resolved);
        }
        if std::fs::symlink_metadata(&existing).is_ok_and(|found| found.file_type().is_symlink()) {
            return None;
        }
        let name = existing.file_name()?.to_owned();
        if !matches!(
            Path::new(&name).components().next(),
            Some(Component::Normal(_))
        ) {
            return None;
        }
        rest.push(name);
        existing = existing.parent()?.to_owned();
    }
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

/// `program args...` confined by `policy`. A detached command outlives the
/// daemon, so its sandbox doesn't die with it.
pub fn command(
    policy: &Policy,
    program: &str,
    args: &[&str],
    detached: bool,
) -> Result<tokio::process::Command> {
    #[cfg(target_os = "linux")]
    {
        linux::command(policy, program, args, detached)
    }
    #[cfg(target_os = "macos")]
    {
        let _ = detached;
        macos::command(policy, program, args)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (policy, program, args, detached);
        available().map(|()| unreachable!())
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{Policy, depth};
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

    fn bwrap() -> Option<PathBuf> {
        BWRAP
            .iter()
            .map(PathBuf::from)
            .find(|found| found.is_file())
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
                BWRAP.join(", ")
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
            let mut kept: Vec<&PathBuf> = root.read_only.iter().collect();
            kept.sort_by_key(|path| depth(path));
            for kept in kept {
                args.extend(["--ro-bind".into(), path(kept), path(kept)]);
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
        // The daemon's socket is out of reach, as Codex keeps its own from
        // a sandboxed command: nothing confined may drive the daemon.
        if let Some(socket) = &policy.socket {
            args.extend(["--ro-bind".into(), "/dev/null".into(), path(socket)]);
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
    ) -> Result<tokio::process::Command> {
        let mut command = tokio::process::Command::new(checked()?);
        command
            .args(args(policy, detached))
            .arg("--")
            .arg(program)
            .args(rest);
        Ok(command)
    }
}

/// The placeholders that running sandboxes hold for missing protected
/// folders: how many hold each, and whether it is being removed. bubblewrap
/// makes a placeholder on the host for its read-only mount; removing it
/// while any sandbox still mounts it would lift that mount there, so the
/// last one out removes it, and a sandbox that needs one being removed waits
/// until it is gone. The lock covers only this list, never the filesystem.
static HELD: std::sync::Mutex<Vec<Placeholder>> = std::sync::Mutex::new(Vec::new());
/// Woken when placeholders being removed are gone.
static REMOVED: tokio::sync::Notify = tokio::sync::Notify::const_new();

struct Placeholder {
    path: PathBuf,
    holders: usize,
    removing: bool,
}

/// Whether a running sandbox holds `path` as a placeholder, or it is being
/// removed: then it is still missing, whatever is there now.
fn held(path: &Path) -> bool {
    HELD.lock().unwrap().iter().any(|held| held.path == path)
}

/// What a running sandbox holds in place; dropping it lets the
/// placeholders go once no other sandbox holds them.
pub struct Hold(Vec<PathBuf>);

/// Hold `policy`'s placeholders while its command runs, once any of them
/// being removed is gone.
pub async fn hold(policy: &Policy) -> Hold {
    let paths: Vec<PathBuf> = match cfg!(target_os = "linux") {
        true => policy
            .roots
            .iter()
            .flat_map(|root| root.missing.iter().cloned())
            .collect(),
        false => Vec::new(),
    };
    loop {
        let removed = REMOVED.notified();
        tokio::pin!(removed);
        removed.as_mut().enable();
        {
            let mut held = HELD.lock().unwrap();
            let removing = held
                .iter()
                .any(|held| held.removing && paths.contains(&held.path));
            if !removing {
                for path in &paths {
                    match held.iter_mut().find(|held| held.path == *path) {
                        Some(held) => held.holders += 1,
                        None => held.push(Placeholder {
                            path: path.clone(),
                            holders: 1,
                            removing: false,
                        }),
                    }
                }
                return Hold(paths);
            }
        }
        removed.await;
    }
}

/// [`hold`] for a call that may run unconfined.
pub async fn hold_for(policy: Option<&Policy>) -> Option<Hold> {
    match policy {
        Some(policy) => Some(hold(policy).await),
        None => None,
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        let mut released = Vec::new();
        {
            let mut held = HELD.lock().unwrap();
            for path in self.0.drain(..) {
                if let Some(held) = held.iter_mut().find(|held| held.path == path) {
                    held.holders -= 1;
                    if held.holders == 0 {
                        held.removing = true;
                        released.push(path);
                    }
                }
            }
        }
        if released.is_empty() {
            return;
        }
        // Off the runtime thread, and with no lock held: no sandbox takes
        // one up while it is marked as being removed.
        let remove = move || {
            for path in &released {
                // Only an empty one: a placeholder, never what a command
                // outside the sandbox made there since.
                let _ = std::fs::remove_dir(path);
            }
            HELD.lock()
                .unwrap()
                .retain(|held| !(held.removing && released.contains(&held.path)));
            REMOVED.notify_waiters();
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => drop(runtime.spawn_blocking(remove)),
            Err(_) => remove(),
        }
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
        let (profile, params) = super::seatbelt(policy);
        let mut command = tokio::process::Command::new(SANDBOX_EXEC);
        command.arg("-p").arg(profile);
        for (key, value) in params {
            command.arg(format!("-D{key}={}", value.to_string_lossy()));
        }
        command.arg("--").arg(program).args(rest);
        Ok(command)
    }
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
        for kept in root.read_only.iter().chain(&root.missing) {
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
        .flat_map(|root| root.read_only.iter().chain(&root.missing))
        .chain(&policy.socket)
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
    // Never the daemon's socket, as Codex keeps its own from a sandboxed
    // command: nothing confined may drive the daemon.
    if let Some(socket) = &policy.socket {
        let key = param(socket);
        sections.push(format!(
            "(deny network-outbound (remote unix-socket (subpath (param \"{key}\"))))"
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
        // Read-only writes nowhere, so every write asks.
        assert!(super::escapes(
            Mode::ReadOnly,
            Some(root),
            &[],
            "write",
            r#"{"path":"a.txt","content":""}"#,
            root
        ));
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
            &Daemon::default(),
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
            helper: None,
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
                    read_only: PROTECTED.iter().map(|name| path.join(name)).collect(),
                    missing: Vec::new(),
                    path: path.clone(),
                }
            })
            .collect();
        let policy = Policy {
            roots,
            network: true,
            socket: Some(path.join("agent.sock")),
            helper: None,
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
            &Daemon::default(),
        )
        .unwrap_err();
        assert_eq!(error.code, "sandbox_unavailable");
        std::fs::remove_dir_all(base).unwrap();
    }
}
