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
use agent_client::files::real;
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
}

/// What a sandboxed command may do: write its roots, and reach the network
/// when `network` is on.
#[derive(Debug, Clone, PartialEq, Eq)]
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
        let mut roots = Vec::new();
        for path in paths {
            roots.push(root(&path, daemon)?);
        }
        Ok(Some(Policy {
            roots,
            network: network && mode == Mode::WorkspaceWrite,
            socket: daemon.socket.clone(),
            capability: None,
            helper: daemon.helper.clone(),
            sockets: Vec::new(),
        }))
    }

    /// This policy, its commands also reaching the Unix sockets at or under
    /// `paths`, absolute paths its settings name.
    pub fn with_sockets(mut self, paths: &[String]) -> Policy {
        self.sockets.extend(paths.iter().map(PathBuf::from));
        self
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
                    .chain(&root.left)
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

/// Whether `command` runs nothing but commands `unsandboxed` lets out of
/// the sandbox, as a Codex rule that allows a command prefix lets it out
/// without asking: each entry is a prefix of words, such as `mcpx` or
/// `docker compose`. Such a command, as a tool with a daemon of its own
/// needs, runs where the bot's client trusted it to. Only a plain script
/// is read, as Codex reads one: words, quoted or not, in commands joined by
/// `&&`, `||`, `;`, `|` or a newline, every one of them a match. Anything
/// that could run something else (an expansion, a substitution, a
/// redirection, a subshell, a command in the background) and anything
/// ill-formed is no match, and the command stays in the sandbox.
pub fn unsandboxed(unsandboxed: &[String], command: &str) -> bool {
    let Some(commands) = commands(command) else {
        return false;
    };
    let prefixes: Vec<Vec<&str>> = unsandboxed
        .iter()
        .map(|prefix| prefix.split_whitespace().collect())
        .filter(|words: &Vec<&str>| !words.is_empty())
        .collect();
    !commands.is_empty()
        && commands.iter().all(|words| {
            prefixes.iter().any(|prefix| {
                words.len() >= prefix.len() && words.iter().zip(prefix).all(|(a, b)| a == b)
            })
        })
}

/// The words of each command in a plain script, as bash reads them; `None`
/// for anything else.
fn commands(script: &str) -> Option<Vec<Vec<String>>> {
    let mut commands = Vec::new();
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut chars = script.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => words.extend(word.take()),
            '\n' | ';' | '&' | '|' => {
                match c {
                    // Alone, `&` puts a command in the background.
                    '&' if chars.next_if_eq(&'&').is_none() => return None,
                    '|' => drop(chars.next_if_eq(&'|')),
                    _ => {}
                }
                words.extend(word.take());
                if !words.is_empty() {
                    commands.push(std::mem::take(&mut words));
                }
            }
            '\'' => {
                let word = word.get_or_insert_default();
                loop {
                    match chars.next()? {
                        '\'' => break,
                        c => word.push(c),
                    }
                }
            }
            '"' => {
                let word = word.get_or_insert_default();
                loop {
                    match chars.next()? {
                        '"' => break,
                        '$' | '`' => return None,
                        '\\' => match chars.next()? {
                            '\n' => {}
                            c @ ('"' | '\\' | '$' | '`') => word.push(c),
                            c => word.extend(['\\', c]),
                        },
                        c => word.push(c),
                    }
                }
            }
            '\\' => match chars.next()? {
                '\n' => {}
                c => word.get_or_insert_default().push(c),
            },
            // A comment, which could hide what a reader of the call misses.
            '#' if word.is_none() => return None,
            '$' | '`' | '<' | '>' | '(' | ')' => return None,
            c => word.get_or_insert_default().push(c),
        }
    }
    words.extend(word);
    if !words.is_empty() {
        commands.push(words);
    }
    Some(commands)
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
}

impl Daemon {
    /// Bot `bot`'s own socket, opened on first use; `None` when the daemon
    /// has no socket for a sandboxed command to use.
    pub fn capability(&self, bot: i64) -> Result<Option<PathBuf>> {
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
    pub open: std::sync::Arc<dyn Fn(i64) -> Result<PathBuf> + Send + Sync>,
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
fn root(path: &Path, daemon: &Daemon) -> Result<Root> {
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
    let mut read_only = Vec::new();
    let mut missing = Vec::new();
    let mut left = Vec::new();
    let names = (daemon.protected.iter().map(|name| (name, true)))
        .chain(daemon.protected_existing.iter().map(|name| (name, false)));
    for (name, kept_missing) in names {
        let kept = path.join(name);
        if held(&kept) {
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
    let git = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".git"));
    if git && !git_folder_writable(path) {
        read_only.push(path.to_owned());
    } else if git {
        for name in GIT_PROTECTED {
            let kept = path.join(name);
            if held(&kept) {
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
        // Each linked worktree's folder names the repository's folder for
        // it the same way. One without it was made since, and serves only a
        // checkout the bot made to name it. Nothing this deep is removed
        // after a command, as the root's own `commondir` is: its folder
        // could be swapped for a link elsewhere before the removal follows.
        for entry in std::fs::read_dir(path.join("worktrees"))
            .into_iter()
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
        read_only.extend(store_files(store).filter(|file| file.exists()));
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
    Ok(Root {
        path: path.to_owned(),
        read_only,
        missing,
        left,
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
        use std::os::fd::AsRawFd;
        let mut command = tokio::process::Command::new(checked()?);
        let filter = loaded(&filter(policy)?)?;
        command
            .args(args(policy, detached))
            .arg("--seccomp")
            .arg(filter.as_raw_fd().to_string());
        let mut kept = vec![filter];
        if let Some(handing) = (policy.capability.as_deref().map(handing).transpose()?).flatten() {
            command.env(
                agent_client::socket::HANDED,
                handing.as_raw_fd().to_string(),
            );
            kept.push(handing);
        }
        command.arg("--").arg(program).args(rest);
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
        Ok(command)
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
    /// with a connection the daemon opens to the bot's own socket, `own`,
    /// for as long as any process of the command holds it. Without a
    /// runtime to answer on, none.
    fn handing(own: &Path) -> Result<Option<std::os::fd::OwnedFd>> {
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
        let own = own.to_owned();
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
                // Handed blocking, as a connection made in the command
                // would be: the description is shared, and its flags with it.
                let stream = (tokio::net::UnixStream::connect(&own).await)
                    .and_then(tokio::net::UnixStream::into_std)
                    .and_then(|stream| stream.set_nonblocking(false).map(|()| stream))
                    .ok();
                let fd = ours.get_ref().as_raw_fd();
                let _ = agent_client::socket::hand(fd, stream.as_ref().map(|s| s.as_raw_fd()));
            }
        });
        Ok(Some(theirs))
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
/// placeholders go once no other sandbox holds them, and removes what the
/// command may not leave made.
pub struct Hold(Vec<PathBuf>, Vec<PathBuf>);

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
                let left = policy
                    .roots
                    .iter()
                    .flat_map(|root| root.left.iter().cloned())
                    .collect();
                return Hold(paths, left);
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
        let left = std::mem::take(&mut self.1);
        if released.is_empty() && left.is_empty() {
            return;
        }
        // Off the runtime thread, and with no lock held: no sandbox takes
        // one up while it is marked as being removed. On a thread of its
        // own, since a runtime shutting down drops blocking work not yet
        // started, which would leave them in place.
        let remove = move || {
            // Git never makes these in a repository's own folder: one there
            // now is the command's.
            for path in &left {
                let _ = std::fs::remove_file(path);
            }
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

    /// The daemon as the CLI starts it, protecting what its client names.
    fn protecting() -> Daemon {
        Daemon {
            protected: agent_client::PROTECTED.map(String::from).to_vec(),
            protected_existing: agent_client::PROTECTED_EXISTING.map(String::from).to_vec(),
            ..Daemon::default()
        }
    }

    #[test]
    fn only_a_plain_script_of_allowed_commands_leaves_the_sandbox() {
        let allowed = ["mcpx".to_owned(), "docker  compose".to_owned()];
        for command in [
            "mcpx",
            "mcpx github search_issues --query 'is:open label:\"bug\"'",
            "mcpx a && mcpx b || mcpx c; mcpx d | mcpx e\nmcpx f",
            "'mcpx' \"x y\" a\\ b \\\n  --flag=1 2>x#y",
            "docker compose up",
            "mcpx a;;",
        ] {
            let command = command.replace("2>x#y", "x#y");
            assert!(unsandboxed(&allowed, &command), "{command}");
        }
        for command in [
            "",
            "  ;",
            "mcpx a | jq .",
            "docker ps",
            "docker",
            "mcpxx a",
            "/usr/bin/mcpx a",
            "PATH=. mcpx a",
            "mcpx a & evil",
            "mcpx a |& evil",
            "mcpx $(evil)",
            "mcpx `evil`",
            "mcpx \"$(evil)\"",
            "mcpx \"`evil`\"",
            "mcpx $HOME",
            "mcpx > ~/.zshrc",
            "mcpx < input",
            "mcpx <(evil)",
            "(mcpx a)",
            "mcpx a # ; evil",
            "mcpx 'unclosed",
            "mcpx \"unclosed",
            "mcpx a\\",
        ] {
            assert!(!unsandboxed(&allowed, command), "{command}");
        }
        assert!(!unsandboxed(&[], "mcpx a"));
        assert!(!unsandboxed(&[" ".to_owned()], "mcpx a"));
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
        assert!(policy.can_write(&git.join("worktrees/wt/index.lock")));
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
                    path: path.clone(),
                }
            })
            .collect();
        let policy = Policy {
            roots,
            network: true,
            socket: Some(path.join("agent.sock")),
            capability: None,
            helper: None,
            sockets: Vec::new(),
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
}
