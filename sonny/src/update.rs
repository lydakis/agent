//! `sonny update`: Sonny and its daemon on the latest release, from one
//! command the person runs, as `codex update` and `claude update` update
//! theirs. It tells how Sonny was installed from where its program is, as
//! Codex does: Homebrew's Cellar (`brew upgrade` of the `sonny` and `agent`
//! formulas, or `brew reinstall` of `sonny-nightly`), or the release
//! tarball install.sh unpacks under `PREFIX/opt` (fetched, checked and
//! linked as install.sh does). Sonny built by hand is left to how it was
//! built. One update runs at a time, under a lock, as Claude Code's.
//!
//! Then the new Sonny does the rest, run in place of the old one: the
//! daemon running now is replaced by the new build's (`agent start
//! --replace`: the old one stops at once, its running turns park and
//! continue on the new one), the companions start on the new build, and
//! what names Sonny's program (the scripts in `~/.agent`, automations, the
//! scheduler's job) is written again by the path that outlives a release.
//! Open windows reopen on the new Sonny themselves as they reconnect
//! ([`installed_since`]). A line says each step, and the last the version.
use std::path::{Path, PathBuf};

pub const USAGE: &str = "usage: sonny update [--download] [--store PATH] [--socket PATH]\n\
     Updates Sonny and its daemon to the latest release, as it was installed (Homebrew, or the\n\
     release tarball install.sh unpacks), then replaces the running daemon with the new one (its\n\
     running turns continue on it) and has open windows reopen on it. --download only fetches\n\
     and checks the release tarball, switching nothing; a later sonny update switches to it.";

/// Unlisted: how the Sonny that installed a release has it do the rest,
/// naming the version it updated from. Kept as it is across releases: an
/// older Sonny runs the newer one with it.
const INSTALLED: &str = "--installed";

const REPO: &str = "https://github.com/lydakis/agent";
/// The Homebrew formulas a stable install has: Sonny's, and the daemon's.
const FORMULAS: [&str; 2] = ["lydakis/agent/sonny", "lydakis/agent/agent"];
const NIGHTLY: &str = "lydakis/agent/sonny-nightly";

/// This Sonny's release.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How Sonny was installed, from where its program is.
#[derive(Debug, PartialEq)]
enum Install {
    /// Homebrew's `sonny` formula, under `PREFIX/Cellar`.
    Brew { prefix: PathBuf },
    /// Homebrew's `sonny-nightly` formula, built from main.
    Nightly { prefix: PathBuf },
    /// The release tarball, unpacked to `PREFIX/opt/sonny-VERSION-linux-ARCH`
    /// and linked by `PREFIX/opt/sonny`.
    Tarball { prefix: PathBuf, arch: String },
    /// Anything else: a checkout's build, `cargo install`, a copy.
    Other,
}

/// How the Sonny at `exe`, its links resolved, was installed.
fn install_of(exe: &Path) -> Install {
    let parts: Vec<&std::ffi::OsStr> = exe.iter().collect();
    if let Some(at) = parts.iter().position(|part| *part == "Cellar")
        && at + 3 < parts.len()
    {
        let prefix: PathBuf = parts[..at].iter().collect();
        if parts[at + 1] == "sonny" {
            return Install::Brew { prefix };
        }
        if parts[at + 1] == "sonny-nightly" {
            return Install::Nightly { prefix };
        }
    }
    let named =
        |path: Option<&Path>, name: &str| path.and_then(Path::file_name).is_some_and(|f| f == name);
    let bin = exe.parent();
    let unpacked = bin.and_then(Path::parent);
    let opt = unpacked.and_then(Path::parent);
    if named(bin, "bin")
        && named(opt, "opt")
        && let Some((_, arch)) = (unpacked.and_then(Path::file_name))
            .and_then(|name| name.to_str())
            .and_then(release_of)
        && let Some(prefix) = opt.and_then(Path::parent)
    {
        return Install::Tarball {
            prefix: prefix.to_owned(),
            arch: arch.to_owned(),
        };
    }
    Install::Other
}

/// The version and architecture a release tarball's folder names:
/// `sonny-VERSION-linux-ARCH`, as install.sh unpacks it.
pub(crate) fn release_of(name: &str) -> Option<(&str, &str)> {
    let (version, arch) = name.strip_prefix("sonny-")?.rsplit_once("-linux-")?;
    let release = version
        .split_once('-')
        .map_or(version, |(release, _)| release);
    (release.split('.').count() == 3
        && release.split('.').all(|n| n.parse::<u64>().is_ok())
        && matches!(arch, "x86_64" | "aarch64"))
    .then_some((version, arch))
}

/// The folder a release tarball unpacks to.
fn tarball_name(version: &str, arch: &str) -> String {
    format!("sonny-{version}-linux-{arch}")
}

/// Whether `latest` is a newer release than `current`, as Codex compares
/// them: plain `MAJOR.MINOR.PATCH` only, so a prerelease is never offered.
fn is_newer(latest: &str, current: &str) -> Option<bool> {
    Some(parse_version(latest)? > parse_version(current)?)
}

fn parse_version(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some((major, minor, patch))
}

/// `sonny update ARGS`.
pub fn cli(args: &[String]) -> Result<(), String> {
    let (mut store, mut socket, mut download, mut installed) = (None, None, false, None);
    let mut passed = Vec::new();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let mut value = || rest.next().cloned().ok_or_else(|| USAGE.to_owned());
        match arg.as_str() {
            "--store" => {
                let path = value()?;
                passed.extend(["--store".to_owned(), path.clone()]);
                store = Some(PathBuf::from(path));
            }
            "--socket" => {
                let path = value()?;
                passed.extend(["--socket".to_owned(), path.clone()]);
                socket = Some(PathBuf::from(path));
            }
            "--download" => download = true,
            INSTALLED => installed = Some(value()?),
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ => return Err(USAGE.into()),
        }
    }
    // Replacing the daemon waits on every running turn, the asking
    // agent's own too: the person runs it, from a terminal of their own.
    if std::env::var_os("AGENT_BOT_ID").is_some() {
        return Err(
            "sonny update replaces the daemon this agent's turn runs on; run it in a terminal of your own"
                .into(),
        );
    }
    let _lock = Lock::take()?;
    let (store, socket) = crate::daemon_paths(store, socket)?;
    if let Some(from) = installed {
        return finish(Last::From(&from), &store, &socket);
    }
    let me = crate::named::me().map_err(|e| e.to_string())?;
    let real = std::fs::canonicalize(&me).unwrap_or(me);
    // Read before anything is installed: an upgrade may remove the program
    // this one runs from, and the link is how the new one is reached.
    let stable = crate::skills::sonny_path().unwrap_or_else(|| real.clone());
    println!("Current version: {VERSION}");
    let install = install_of(&real);
    if download && !matches!(install, Install::Tarball { .. }) {
        return Err(
            "--download fetches the release tarball install.sh installs; Homebrew fetches and installs in one step"
                .into(),
        );
    }
    let updated = match &install {
        Install::Brew { prefix } => brew(prefix)?,
        Install::Nightly { prefix } => nightly(prefix)?,
        Install::Tarball { prefix, arch } => match tarball(prefix, arch, download)? {
            Fetched::Downloaded => return Ok(()),
            Fetched::Linked(link) => Some(link),
            Fetched::Current => None,
        },
        Install::Other => {
            println!(
                "Could not tell how Sonny at {} was installed; update it the way you installed it",
                crate::tilde(&real.to_string_lossy())
            );
            return finish(Last::AsBuilt, &store, &socket);
        }
    };
    let Some(new) = updated else {
        return finish(Last::Current, &store, &socket);
    };
    // The new Sonny does the rest, which only it knows how to do.
    let new = match install {
        Install::Tarball { .. } => new,
        _ => stable,
    };
    if same_file(&new, &real) {
        return finish(Last::Current, &store, &socket);
    }
    use std::os::unix::process::CommandExt;
    let error = std::process::Command::new(&new)
        .arg("update")
        .args([INSTALLED, VERSION])
        .args(&passed)
        .exec();
    Err(format!("{}: {error}", new.display()))
}

/// Whether `a` and `b` are one file.
fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let identity = |path: &Path| std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()));
    identity(a).is_some_and(|a| Some(a) == identity(b))
}

/// Homebrew's `brew`: the one beside the Cellar Sonny is in, else PATH's.
fn brew_at(prefix: &Path) -> PathBuf {
    let beside = prefix.join("bin/brew");
    match beside.is_file() {
        true => beside,
        false => PathBuf::from("brew"),
    }
}

/// Run `program ARGS`, its output the person's, as Codex runs an update:
/// from `/`, so no project's own configuration or hooks take part.
fn run(program: &Path, args: &[&str]) -> Result<(), String> {
    let shown = std::iter::once(program.to_string_lossy().into_owned())
        .chain(args.iter().map(|arg| (*arg).to_owned()))
        .collect::<Vec<_>>()
        .join(" ");
    let status = std::process::Command::new(program)
        .args(args)
        .current_dir("/")
        // `brew update` ran just before, when it is brew.
        .env("HOMEBREW_NO_AUTO_UPDATE", "1")
        .status()
        .map_err(|e| format!("`{shown}`: {e}"))?;
    match status.success() {
        true => Ok(()),
        false => Err(format!("`{shown}` failed with {status}")),
    }
}

/// The stable formulas brought up to date: `brew update`, then `brew
/// upgrade` when either is behind. Some path when one was installed.
fn brew(prefix: &Path) -> Result<Option<PathBuf>, String> {
    let brew = brew_at(prefix);
    println!("Checking for updates...");
    // The tap is a repository of its own, which only an update reads again:
    // a release made since the last one is not seen without it.
    run(&brew, &["update", "--quiet"])?;
    let outdated = std::process::Command::new(&brew)
        .args(["outdated", "--json=v2"])
        .args(FORMULAS)
        .current_dir("/")
        .env("HOMEBREW_NO_AUTO_UPDATE", "1")
        .stderr(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("{}: {e}", brew.display()))?;
    // A listing brew could not give is no proof of being current: the
    // upgrade itself then says what is behind.
    let listed: Option<serde_json::Value> = serde_json::from_slice(&outdated.stdout).ok();
    if let Some(listed) = &listed {
        let behind: Vec<&serde_json::Value> = (listed["formulae"].as_array().into_iter())
            .flatten()
            .filter(|formula| formula["current_version"].is_string())
            .collect();
        if behind.is_empty() {
            return Ok(None);
        }
        let sonny = behind.iter().find(|formula| formula["name"] == "sonny");
        if let Some(latest) = sonny.and_then(|formula| formula["current_version"].as_str()) {
            println!("New version available: {latest} (current: {VERSION})");
        }
    }
    let mut args = vec!["upgrade"];
    args.extend(FORMULAS);
    println!("Updating Sonny via `brew {}`...", args.join(" "));
    run(&brew, &args)?;
    Ok(Some(PathBuf::new()))
}

/// The nightly formula rebuilt from main, when main moved past the commit
/// this Sonny was built from.
fn nightly(prefix: &Path) -> Result<Option<PathBuf>, String> {
    let brew = brew_at(prefix);
    println!("Checking for updates...");
    let main = std::process::Command::new("git")
        .args(["ls-remote", REPO, "refs/heads/main"])
        .current_dir("/")
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .and_then(|out| out.split_whitespace().next().map(str::to_owned));
    if let (Some(main), Some(built)) = (&main, option_env!("SONNY_COMMIT"))
        && !built.is_empty()
        && main.starts_with(built)
    {
        return Ok(None);
    }
    if let Some(main) = &main {
        println!("New commit on main: {}", &main[..main.len().min(9)]);
    }
    run(&brew, &["update", "--quiet"])?;
    println!("Updating Sonny via `brew reinstall {NIGHTLY}`...");
    run(&brew, &["reinstall", NIGHTLY])?;
    Ok(Some(PathBuf::new()))
}

/// What fetching the release tarball came to.
enum Fetched {
    /// This Sonny is the latest.
    Current,
    /// Unpacked and checked, not linked: `--download`.
    Downloaded,
    /// Linked, by `PREFIX/opt/sonny/bin/sonny`.
    Linked(PathBuf),
}

/// The latest release's tarball, as install.sh installs it: downloaded,
/// checked against its sha256 and, when the GitHub CLI is signed in, the
/// release workflow's build attestation, unpacked to
/// `PREFIX/opt/sonny-VERSION-linux-ARCH`, then `PREFIX/opt/sonny` and
/// `PREFIX/bin/sonny` linked to it. Earlier versions stay, as install.sh
/// leaves them. One a `--download` unpacked already is not fetched again.
fn tarball(prefix: &Path, arch: &str, download: bool) -> Result<Fetched, String> {
    println!("Checking for updates...");
    let latest = latest_release()?;
    match is_newer(&latest, VERSION) {
        Some(true) => println!("New version available: {latest} (current: {VERSION})"),
        Some(false) if latest == VERSION => return Ok(Fetched::Current),
        _ => {
            println!(
                "You're running {VERSION}, which is not older than the latest release's {latest}. Skipping update."
            );
            return Ok(Fetched::Current);
        }
    }
    let name = tarball_name(&latest, arch);
    let opt = prefix.join("opt");
    let unpacked = opt.join(&name);
    if unpacked.join("bin/sonny").is_file() && unpacked.join("libexec/agent").is_file() {
        println!(
            "Found {}, downloaded and checked before",
            crate::tilde(&unpacked.to_string_lossy())
        );
    } else {
        fetch(&opt, &name, &latest)?;
        println!("Unpacked to {}", crate::tilde(&unpacked.to_string_lossy()));
    }
    if download {
        println!("Downloaded {latest} without switching to it; sonny update switches");
        return Ok(Fetched::Downloaded);
    }
    let link = |path: &Path, target: &str| -> Result<(), String> {
        // Made beside it and moved over it, so `sonny` is never missing.
        let made = path.with_file_name(format!(".sonny-update-{}", std::process::id()));
        let _ = std::fs::remove_file(&made);
        std::os::unix::fs::symlink(target, &made)
            .and_then(|()| std::fs::rename(&made, path))
            .map_err(|e| {
                let _ = std::fs::remove_file(&made);
                format!("{}: {e}", path.display())
            })
    };
    link(&opt.join("sonny"), &name)?;
    std::fs::create_dir_all(prefix.join("bin"))
        .map_err(|e| format!("{}: {e}", prefix.join("bin").display()))?;
    link(&prefix.join("bin/sonny"), "../opt/sonny/bin/sonny")?;
    println!(
        "Linked {} and {} to {latest}",
        crate::tilde(&opt.join("sonny").to_string_lossy()),
        crate::tilde(&prefix.join("bin/sonny").to_string_lossy())
    );
    Ok(Fetched::Linked(opt.join("sonny/bin/sonny")))
}

/// The latest release's version: where its page redirects, as install.sh
/// finds it.
fn latest_release() -> Result<String, String> {
    let out = std::process::Command::new("curl")
        .args(["-fsSLI", "-o", "/dev/null", "-w", "%{url_effective}"])
        .arg(format!("{REPO}/releases/latest"))
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("Failed to check for updates: curl: {e}"))?;
    let url = String::from_utf8_lossy(&out.stdout);
    let version = url.rsplit_once("/v").map(|(_, version)| version.trim());
    match version {
        Some(version) if out.status.success() && parse_version(version).is_some() => {
            Ok(version.to_owned())
        }
        _ => Err(format!(
            "Failed to check for updates: no release found at {REPO}/releases/latest ({})",
            String::from_utf8_lossy(&out.stderr).trim()
        )),
    }
}

/// Download `name`'s tarball for `version` to a folder of its own in
/// `opt`, check it, and move what it unpacks to `opt/name`.
fn fetch(opt: &Path, name: &str, version: &str) -> Result<(), String> {
    let work = opt.join(format!(".sonny-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).map_err(|e| format!("{}: {e}", work.display()))?;
    let result = fetch_in(&work, opt, name, version);
    let _ = std::fs::remove_dir_all(&work);
    result
}

fn fetch_in(work: &Path, opt: &Path, name: &str, version: &str) -> Result<(), String> {
    let url = format!("{REPO}/releases/download/v{version}/{name}.tar.gz");
    let archive = work.join(format!("{name}.tar.gz"));
    let sum = work.join(format!("{name}.tar.gz.sha256"));
    println!("Downloading {name}");
    for (from, to) in [(url.clone(), &archive), (format!("{url}.sha256"), &sum)] {
        let fetched = std::process::Command::new("curl")
            .args(["-fsSL", "-o"])
            .arg(to)
            .arg(&from)
            .stdin(std::process::Stdio::null())
            .status()
            .map_err(|e| format!("curl: {e}"))?;
        if !fetched.success() {
            return Err(format!("could not download {from}"));
        }
    }
    let published = std::fs::read_to_string(&sum).map_err(|e| format!("{}: {e}", sum.display()))?;
    let published = published.split_whitespace().next().unwrap_or_default();
    if !published.eq_ignore_ascii_case(&sha256(&archive)?) {
        return Err(format!("{name}.tar.gz does not match its published sha256"));
    }
    let gh_signed_in = std::process::Command::new("gh")
        .args(["auth", "status"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if gh_signed_in {
        let attested = std::process::Command::new("gh")
            .args(["attestation", "verify"])
            .arg(&archive)
            .args([
                "--repo",
                "lydakis/agent",
                "--signer-workflow",
                "lydakis/agent/.github/workflows/release.yml",
                "--deny-self-hosted-runners",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if !attested {
            return Err(format!(
                "{name}.tar.gz has no build attestation from the release workflow"
            ));
        }
        println!("Checked its sha256 and the release workflow's build attestation");
    } else {
        println!(
            "Checked its sha256 (with the GitHub CLI signed in, its build attestation is checked too)"
        );
    }
    let unpacked = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(work)
        .status()
        .map_err(|e| format!("tar: {e}"))?;
    let laid_out = |rest: &str| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(work.join(name).join(rest))
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if !unpacked.success() || !laid_out("bin/sonny") || !laid_out("libexec/agent") {
        return Err(format!(
            "{name}.tar.gz is not laid out as bin/sonny and libexec/agent"
        ));
    }
    let to = opt.join(name);
    let _ = std::fs::remove_dir_all(&to);
    std::fs::rename(work.join(name), &to).map_err(|e| format!("{}: {e}", to.display()))
}

/// The SHA-256 of the file at `path`, in hex.
fn sha256(path: &Path) -> Result<String, String> {
    use sha2::Digest;
    let mut file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut hasher = sha2::Sha256::new();
    std::io::copy(&mut file, &mut hasher).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// What the last line of an update says.
enum Last<'a> {
    /// Updated from this version.
    From(&'a str),
    /// Nothing newer was found.
    Current,
    /// Not installed by anything `sonny update` knows: as it was built.
    AsBuilt,
}

/// The rest of an update, by the Sonny now installed: the daemon replaced
/// by this build's, the companions started on it, what names Sonny's
/// program written again, and the last line naming the version.
fn finish(last: Last, store: &Path, socket: &Path) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    // A build just installed is the daemon's to run even when it names the
    // same version: a nightly's next commit does.
    let installed = matches!(last, Last::From(_));
    let replaced = runtime.block_on(switch_daemon(store, socket, installed))?;
    // What runs Sonny by its program's path names the path that outlives a
    // release; the scheduler's job is written again when it ran another.
    crate::skills::install_own();
    crate::trigger::refresh_now();
    if let Some(sonny) = crate::skills::sonny_path() {
        println!(
            "Scripts, automations and the scheduler run {}",
            crate::tilde(&sonny.to_string_lossy())
        );
    }
    if replaced {
        println!(
            "Open Sonny windows reopen on {VERSION} as they reconnect, keeping what you typed"
        );
    }
    match last {
        Last::From(from) if from != VERSION => {
            println!("Successfully updated from {from} to version {VERSION}")
        }
        Last::From(_) => println!("Successfully updated to the newest build of {VERSION}"),
        Last::AsBuilt => println!("Sonny is on version {VERSION}, as built"),
        _ => println!("Sonny is up to date ({VERSION})"),
    }
    Ok(())
}

/// The daemon on `socket` made this build's: replaced when it runs
/// another release, or always when `installed` (a build was just
/// installed), its companions then started on this one. Whether it was
/// replaced.
async fn switch_daemon(store: &Path, socket: &Path, installed: bool) -> Result<bool, String> {
    let agent = crate::agent_bin_or_missing()?;
    let own = agent_version(&agent);
    let running = match agent_client::Client::connect(socket).await {
        Ok((client, _events)) => {
            let version = client.version().map(str::to_owned);
            client.close().await;
            Some(version)
        }
        Err(error) if crate::newer_daemon(&error) => {
            return Err(
                "the daemon running now is newer than this Sonny; update the Sonny that started it"
                    .into(),
            );
        }
        // An older protocol's daemon: replaced as any start replaces it.
        Err(error) if error.code == "daemon_protocol_mismatch" => Some(None),
        Err(_) => None,
    };
    let Some(version) = running else {
        println!("Daemon: not running; the next window starts {own}");
        return Ok(false);
    };
    let replace = installed || version.as_deref() != Some(own.as_str());
    if replace {
        crate::start_daemon_replacing(store, socket).map_err(|e| e.to_string())?;
        println!(
            "Daemon: {} stopped and {own} started; its running turns continue on {own}",
            version.as_deref().unwrap_or("the older daemon")
        );
    } else {
        println!("Daemon: {own} running");
    }
    let (client, _events) = agent_client::Client::connect(socket)
        .await
        .map_err(|e| e.to_string())?;
    crate::start_companions(&client).await?;
    client.close().await;
    println!("Companions: the lookout and tool host run Sonny {VERSION}");
    Ok(replace)
}

/// The release `agent --version` names, else this Sonny's.
fn agent_version(agent: &Path) -> String {
    std::process::Command::new(agent)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .and_then(|out| out.split_whitespace().nth(1).map(str::to_owned))
        .unwrap_or_else(|| VERSION.to_owned())
}

/// One update at a time, by a lock on `~/.agent/sonny/update.lock`, which
/// names the process holding it.
struct Lock {
    _file: std::fs::File,
}

impl Lock {
    fn take() -> Result<Self, String> {
        use std::io::{Read, Seek, Write};
        use std::os::fd::AsRawFd;
        let home = std::env::var_os("HOME").ok_or("no HOME")?;
        let path = PathBuf::from(home).join(".agent/sonny/update.lock");
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        // SAFETY: flock takes the descriptor `file` owns for as long as the
        // call runs.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let mut holder = String::new();
            let _ = file.read_to_string(&mut holder);
            let holder = match holder.trim() {
                "" => String::new(),
                pid => format!(" (pid {pid})"),
            };
            return Err(format!(
                "Another sonny update{holder} is running. Please try again in a moment."
            ));
        }
        let _ = file.set_len(0);
        let _ = file.rewind();
        let _ = write!(file, "{}", std::process::id());
        Ok(Lock { _file: file })
    }
}

/// The program this window started from, as it was then.
/// Its path that outlives a release too, read while the program is there:
/// an upgrade may remove it, after which no path leads from it to the new
/// one.
static STARTED: std::sync::OnceLock<Option<(PathBuf, (u64, u64))>> = std::sync::OnceLock::new();

/// Note the program this window runs, for [`installed_since`].
pub fn note_started() {
    STARTED.get_or_init(|| {
        let me = crate::named::me().ok()?;
        let running = identity(&std::fs::canonicalize(&me).unwrap_or(me))?;
        Some((crate::skills::sonny_path()?, running))
    });
}

fn identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

/// Another Sonny installed where this one runs from since this window
/// started (`sonny update`, `brew upgrade`, install.sh, a rebuild): its
/// program, by the path that outlives a release, which the window
/// reopens on as it reconnects, rather than run the older one on.
pub fn installed_since() -> Option<PathBuf> {
    let (installed, started) = STARTED.get()?.as_ref()?;
    (identity(installed).is_some_and(|now| now != *started)).then(|| installed.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn how_sonny_was_installed_is_told_by_where_it_is() {
        assert_eq!(
            install_of(Path::new("/opt/homebrew/Cellar/sonny/0.9.4/bin/sonny")),
            Install::Brew {
                prefix: PathBuf::from("/opt/homebrew")
            }
        );
        assert_eq!(
            install_of(Path::new(
                "/home/linuxbrew/.linuxbrew/Cellar/sonny-nightly/13.1/bin/sonny"
            )),
            Install::Nightly {
                prefix: PathBuf::from("/home/linuxbrew/.linuxbrew")
            }
        );
        assert_eq!(
            install_of(Path::new(
                "/home/me/.local/opt/sonny-0.9.4-linux-x86_64/bin/sonny"
            )),
            Install::Tarball {
                prefix: PathBuf::from("/home/me/.local"),
                arch: "x86_64".into()
            }
        );
        assert_eq!(
            install_of(Path::new(
                "/srv/opt/sonny-1.0.0-rc.1-linux-aarch64/bin/sonny"
            )),
            Install::Tarball {
                prefix: PathBuf::from("/srv"),
                arch: "aarch64".into()
            }
        );
        // A checkout's build, cargo install, an unpacked tarball elsewhere.
        for other in [
            "/home/me/agent/target/release/sonny",
            "/home/me/.cargo/bin/sonny",
            "/tmp/sonny-0.9.4-linux-x86_64/bin/sonny",
            "/opt/homebrew/Cellar/agent/0.9.4/bin/agent",
        ] {
            assert_eq!(install_of(Path::new(other)), Install::Other, "{other}");
        }
    }

    #[test]
    fn a_release_folder_names_its_version_and_architecture() {
        assert_eq!(
            release_of("sonny-0.9.5-linux-x86_64"),
            Some(("0.9.5", "x86_64"))
        );
        assert_eq!(
            release_of("sonny-1.0.0-rc.1-linux-aarch64"),
            Some(("1.0.0-rc.1", "aarch64"))
        );
        assert_eq!(release_of("sonny-tools"), None);
        assert_eq!(release_of("sonny-0.9-linux-x86_64"), None);
        assert_eq!(release_of("sonny-0.9.5-linux-riscv64"), None);
        assert_eq!(release_of("sonny"), None);
        assert_eq!(tarball_name("0.9.5", "x86_64"), "sonny-0.9.5-linux-x86_64");
    }

    #[test]
    fn only_a_newer_plain_release_is_offered() {
        assert_eq!(is_newer("0.9.5", "0.9.4"), Some(true));
        assert_eq!(is_newer("0.10.0", "0.9.9"), Some(true));
        assert_eq!(is_newer("0.9.4", "0.9.4"), Some(false));
        assert_eq!(is_newer("0.9.3", "0.9.4"), Some(false));
        // A prerelease is never offered, as Codex never offers one.
        assert_eq!(is_newer("0.10.0-rc.1", "0.9.4"), None);
        assert_eq!(is_newer("0.9.5", "0.9.5-rc.1"), None);
    }

    #[test]
    fn a_tarball_is_checked_against_its_published_sha256() {
        let dir = std::env::temp_dir().join(format!("sonny-update-sum-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.tar.gz");
        std::fs::write(&file, "abc").unwrap();
        assert_eq!(
            sha256(&file).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_window_reopens_only_once_another_sonny_is_installed() {
        let dir = std::env::temp_dir().join(format!("sonny-update-since-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (old, new, link) = (dir.join("old"), dir.join("new"), dir.join("link"));
        std::fs::write(&old, "old").unwrap();
        std::fs::write(&new, "new").unwrap();
        std::os::unix::fs::symlink(&old, &link).unwrap();
        let started = identity(&link);
        assert_eq!(identity(&link), started);
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&new, &link).unwrap();
        assert_ne!(identity(&link), started);
        assert!(same_file(&link, &new) && !same_file(&link, &old));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
