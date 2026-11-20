//! `sonny upgrade`: Sonny and its daemon on the latest release, from one
//! command the person runs, as `codex update` and `claude update` update
//! theirs. It tells how Sonny was installed from where its program is, as
//! Codex does: Homebrew's Cellar (`brew upgrade` of the `sonny` and `agent`
//! formulas, or `brew reinstall` of `sonny-nightly`), or the release
//! tarball install.sh unpacks under `PREFIX/opt` (fetched, checked and
//! linked as install.sh does). Sonny built by hand is left to how it was
//! built. One upgrade runs at a time, under a lock, as Claude Code's.
//!
//! Then the new Sonny does the rest, run in place of the old one: the
//! daemon running now is replaced by the new build's (`agent start
//! --replace`: the old one stops at once, its running turns park and
//! continue on the new one), the companions are checked on the new build
//! and replaced where they are not ([`check_companions`]), and
//! what names Sonny's program (the scripts in `~/.agent`, automations, the
//! scheduler's job) is written again by the path that outlives a release,
//! and Sonny's own agent-browser and agent-desktop are brought to the
//! versions its tools list.
//! Open windows reopen on the new Sonny themselves as they reconnect
//! ([`installed_since`]). A line says each step, and the last the version.
use std::path::{Path, PathBuf};

pub const USAGE: &str = "usage: sonny upgrade [--download] [--store PATH] [--socket PATH]\n\
     Upgrades Sonny and its daemon to the latest release, as it was installed (Homebrew, or the\n\
     release tarball install.sh unpacks), then replaces the running daemon with the new one (its\n\
     running turns continue on it) and has open windows reopen on it. --download only fetches\n\
     and checks the release tarball, switching nothing; a later sonny upgrade switches to it.";

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

/// `sonny upgrade ARGS`.
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
            "sonny upgrade replaces the daemon this agent's turn runs on; run it in a terminal of your own"
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
    // The program this is, by its file, before anything is installed: a
    // reinstall in place (a nightly of the same version) puts another file
    // at this same path.
    let running = identity(&real);
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
                "Could not tell how Sonny at {} was installed; upgrade it the way you installed it",
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
    if running.is_some() && identity(&new) == running {
        return finish(Last::Current, &store, &socket);
    }
    use std::os::unix::process::CommandExt;
    let error = std::process::Command::new(&new)
        .arg("upgrade")
        .args([INSTALLED, VERSION])
        .args(&passed)
        .exec();
    Err(format!("{}: {error}", new.display()))
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
    println!("Checking for a new release...");
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
    if let Some(behind) = listed
        .as_ref()
        .and_then(|value| value["formulae"].as_array())
    {
        if outdated.status.success() && behind.is_empty() {
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
    println!("Checking for a new release...");
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
    println!("Checking for a new release...");
    let latest = latest_release()?;
    match is_newer(&latest, VERSION) {
        Some(true) => println!("New version available: {latest} (current: {VERSION})"),
        Some(false) if latest == VERSION => return Ok(Fetched::Current),
        _ => {
            println!(
                "You're running {VERSION}, which is not older than the latest release's {latest}. Skipping the upgrade."
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
        println!("Downloaded {latest} without switching to it; sonny upgrade switches");
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
        .map_err(|e| format!("Failed to check for a new release: curl: {e}"))?;
    let url = String::from_utf8_lossy(&out.stdout);
    let version = url.rsplit_once("/v").map(|(_, version)| version.trim());
    match version {
        Some(version) if out.status.success() && parse_version(version).is_some() => {
            Ok(version.to_owned())
        }
        _ => Err(format!(
            "Failed to check for a new release: no release found at {REPO}/releases/latest ({})",
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
    /// Not installed by anything `sonny upgrade` knows: as it was built.
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
    // Sonny's own files migrated before the daemon and its companions
    // start on this build.
    crate::upgrade::at_start(store);
    let (replaced, failing) = runtime.block_on(switch_daemon(store, socket, installed))?;
    // Sonny's own agent-browser and agent-desktop at the versions this
    // build's tools list, before the skills they serve are written again.
    let say = |line: &str| println!("{line}");
    for error in crate::trusted::bring_up(&say) {
        println!("Not installed: {error}");
    }
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
    // Sonny's new text set beside the person's edits, which a lead is told
    // of when a window next opens: said here too, by name.
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for (word, name, theirs, staged) in crate::skills::staged(&home) {
            println!(
                "Your {name} {word} ({}) is kept; Sonny's new text waits at {}",
                crate::tilde(&theirs.to_string_lossy()),
                crate::tilde(&staged.to_string_lossy())
            );
        }
    }
    match last {
        Last::From(from) if from != VERSION => {
            println!("Successfully upgraded from {from} to version {VERSION}")
        }
        Last::From(_) => println!("Successfully upgraded to the newest build of {VERSION}"),
        Last::AsBuilt => println!("Sonny is on version {VERSION}, as built"),
        _ => println!("Sonny is up to date ({VERSION})"),
    }
    still_failing(&failing)
}

/// The upgrade's outcome once its companions are checked: an error, so
/// it exits non-zero, while any is still failing.
fn still_failing(failing: &[&str]) -> Result<(), String> {
    match failing {
        [] => Ok(()),
        [one] => Err(format!("{one} is still failing on {VERSION}")),
        _ => Err(format!(
            "{} are still failing on {VERSION}",
            failing.join(", ")
        )),
    }
}

/// The daemon on `socket` made this build's: replaced when it runs
/// another release, or always when `installed` (a build was just
/// installed), its companions then checked on this one. Whether it was
/// replaced, and the companions still failing.
async fn switch_daemon(
    store: &Path,
    socket: &Path,
    installed: bool,
) -> Result<(bool, Vec<&'static str>), String> {
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
                "the daemon running now is newer than this Sonny; upgrade the Sonny that started it"
                    .into(),
            );
        }
        // An older protocol's daemon: replaced as any start replaces it.
        Err(error) if error.code == "daemon_protocol_mismatch" => Some(None),
        Err(_) => None,
    };
    let Some(version) = running else {
        println!("Daemon: not running; the next window starts {own}");
        return Ok((false, Vec::new()));
    };
    let replace = installed || version.as_deref() != Some(own.as_str());
    if replace {
        crate::start_daemon_replacing(store, socket).map_err(|e| format!("{e}; {UNCHECKED}"))?;
        println!(
            "Daemon: {} stopped and {own} started; its running turns continue on {own}",
            version.as_deref().unwrap_or("the older daemon")
        );
    } else {
        println!("Daemon: {own} running");
    }
    let (client, _events) = agent_client::Client::connect(socket)
        .await
        .map_err(|e| format!("{e}; {UNCHECKED}"))?;
    let checked = check_companions(&client, store, &Watch::UPGRADE).await;
    client.close().await;
    let checked = checked.map_err(|e| format!("{e}; {UNCHECKED}"))?;
    for (name, found) in &checked {
        println!("{}", said(name, found));
    }
    let failing = (checked.iter())
        .filter(|(_, found)| matches!(found, Checked::Failing(_)))
        .map(|(name, _)| *name)
        .collect();
    Ok((replace, failing))
}

/// Why an upgrade left its companions unchecked: the daemon did not come
/// up on this build, and they come back as they were kept when it does.
const UNCHECKED: &str = "its companions were not checked; once the daemon runs, `sonny upgrade` puts them on this build";

/// What became of one of Sonny's companions at an upgrade.
#[derive(Debug, PartialEq)]
enum Checked {
    /// This build's, running.
    Ok,
    /// Put on this build in place of what it was.
    Replaced(String),
    /// None was kept; this build's was started.
    Started,
    /// Not running on this build, with the daemon's word on why.
    Failing(String),
}

/// How [`check_companions`] watches the companions run.
struct Watch {
    /// How long a run lasts with no restart to count as running: the
    /// daemon starts one that exited again after a second, then two, so
    /// one that exits as it starts has restarted by then.
    settled: std::time::Duration,
    /// How long it waits for them all: one replaced waits for the run
    /// before it to stop, up to the daemon's 5 s grace, then settles.
    until: std::time::Duration,
    /// How often it asks the daemon.
    every: std::time::Duration,
}

impl Watch {
    const UPGRADE: Watch = Watch {
        settled: std::time::Duration::from_secs(5),
        until: std::time::Duration::from_secs(15),
        every: std::time::Duration::from_millis(200),
    };
}

/// One line for what became of companion `name`.
fn said(name: &str, found: &Checked) -> String {
    match found {
        Checked::Ok => format!("Companion {name}: ok"),
        Checked::Replaced(was) => format!("Companion {name}: replaced (was {was})"),
        Checked::Started => format!("Companion {name}: started (none was kept)"),
        Checked::Failing(why) => format!("Companion {name}: still failing ({why})"),
    }
}

/// Every companion of Sonny's the daemon keeps on this build and running:
/// the lookout and the tool host, and the reviewer when one is kept. One
/// of another build, run by a name that led to another program, or not
/// running is replaced with this build's, and each is watched until it
/// has run [`Watch::settled`] with no restart, or [`Watch::until`] is up.
/// A daemon started after an upgrade by anything but this Sonny (the
/// `agent` CLI, a window that only joined it) brings back the companions
/// it kept, by names that still lead to the program they named then.
async fn check_companions(
    client: &agent_client::Client,
    store: &Path,
    watch: &Watch,
) -> Result<Vec<(&'static str, Checked)>, String> {
    let me = crate::named::me().map_err(|e| e.to_string())?;
    let build = crate::lookout::identity();
    let before = crate::companions(client).await?;
    let reviewer = [crate::REVIEWER, crate::OLD_REVIEWER]
        .iter()
        .any(|name| crate::kept_companion(&before, name).is_some());
    let mut names = vec![
        (crate::LOOKOUT, crate::named::LOOKOUT),
        (crate::TOOL_HOST, crate::named::TOOL_HOST),
    ];
    if reviewer {
        names.push((crate::REVIEWER, crate::named::REVIEWER));
    }
    // Why each needs replacing, read before anything is linked again.
    let now = now_ms();
    let was: Vec<Option<String>> = (names.iter())
        .map(
            |(name, program)| match crate::kept_companion(&before, name) {
                None if *name == crate::REVIEWER => {
                    crate::kept_companion(&before, crate::OLD_REVIEWER)
                        .map(|_| "kept by its name before 0.9.1".to_owned())
                }
                None => None,
                Some(kept) => off_build(kept, &me, program, &build).or_else(|| unwell(kept, now)),
            },
        )
        .collect();
    // Each put on this build: one whose spec changes starts again.
    crate::start_companions(client, crate::Companions::Replace).await?;
    let after = crate::companions(client).await?;
    for ((name, _), was) in names.iter().zip(&was) {
        let spec = |listed: &serde_json::Value| {
            crate::kept_companion(listed, name)
                .map(|c| [&c["command"], &c["args"], &c["env"], &c["cwd"]].map(Clone::clone))
        };
        // The same spec as before keeps the run it has, from the program it
        // started from: stopped and started again.
        if was.is_some()
            && let Some(kept) = crate::kept_companion(&after, name)
            && spec(&before) == spec(&after)
        {
            restart(client, kept).await?;
        }
    }
    // Read again: a run restarted above is listed by its replacement only
    // from now on.
    let mut listed = crate::companions(client).await?;
    let mut settled = vec![false; names.len()];
    let deadline = tokio::time::Instant::now() + watch.until;
    loop {
        let now = now_ms();
        for ((name, program), settled) in names.iter().zip(settled.iter_mut()) {
            *settled = crate::kept_companion(&listed, name).is_some_and(|kept| {
                off_build(kept, &me, program, &build).is_none()
                    && kept["running"] == true
                    && (kept["started_ms"].as_u64()).is_some_and(|at| {
                        now.saturating_sub(at) >= watch.settled.as_millis() as u64
                    })
            });
        }
        if settled.iter().all(|s| *s) || tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(watch.every).await;
        listed = crate::companions(client).await?;
    }
    let logs = format!("{}.companions", store.display());
    Ok((names.iter().zip(was).zip(settled))
        .map(|(((name, program), was), settled)| {
            let found = match (settled, was) {
                (true, None) if crate::kept_companion(&before, name).is_none() => Checked::Started,
                (true, None) => Checked::Ok,
                (true, Some(was)) => Checked::Replaced(was),
                (false, _) => Checked::Failing(failing(
                    crate::kept_companion(&listed, name),
                    &me,
                    program,
                    &build,
                    &format!("{}/{name}.log", crate::tilde(&logs)),
                )),
            };
            (*name, found)
        })
        .collect())
}

/// How companion `kept` is not this build's, run by program `me`'s name
/// `program`: none when it is.
fn off_build(kept: &serde_json::Value, me: &Path, program: &str, build: &str) -> Option<String> {
    let theirs = kept["env"]["SONNY_BUILD"].as_str();
    if theirs != Some(build) {
        let version = |build: &str| build.split(' ').next().unwrap_or_default().to_owned();
        return Some(match theirs.map(version) {
            None => "a build that does not name itself".to_owned(),
            Some(theirs) if theirs == VERSION => format!("another build of {VERSION}"),
            Some(theirs) => theirs,
        });
    }
    let command = Path::new(kept["command"].as_str().unwrap_or_default());
    let named = crate::named::path(program);
    if command == named && !crate::named::current(me, program) {
        return Some(format!("an older program by its name, {}", tilde(&named)));
    }
    if command != named && !crate::named::runs(command, me) {
        return Some(tilde(command));
    }
    None
}

/// How companion `kept` is not running well as of `now`: down, or
/// restarted and up for less than a healthy run, as `/status` alerts.
fn unwell(kept: &serde_json::Value, now: u64) -> Option<String> {
    let restarts = kept["restarts"].as_u64().unwrap_or(0);
    let up = (kept["started_ms"].as_u64()).map(|at| now.saturating_sub(at));
    let how = || crate::status::companion_exit(&kept["last_exit"]);
    match kept["running"] == true {
        false if kept["last_exit"].is_null() => Some("not running".to_owned()),
        false => Some(format!("not running: {}", how())),
        true if restarts > 0 && up.is_none_or(|up| up < crate::status::HEALTHY_MS) => Some(
            format!("restarting: {}, {}", how(), restarts_said(restarts)),
        ),
        true => None,
    }
}

/// Why companion `kept`, watched, is still not running on this build.
fn failing(
    kept: Option<&serde_json::Value>,
    me: &Path,
    program: &str,
    build: &str,
    log: &str,
) -> String {
    let Some(kept) = kept else {
        return "the daemon keeps none".to_owned();
    };
    if let Some(theirs) = off_build(kept, me, program, build) {
        return format!("still on {theirs}");
    }
    let restarts = kept["restarts"].as_u64().unwrap_or(0);
    let how = match kept["last_exit"].is_null() {
        true => "not started yet".to_owned(),
        false => crate::status::companion_exit(&kept["last_exit"]),
    };
    let detail = (kept["last_exit"]["detail"].as_str()).map_or(String::new(), |d| format!(": {d}"));
    match restarts {
        0 => format!("{how}{detail}; its log: {log}"),
        _ => format!("{how}{detail}, {}; its log: {log}", restarts_said(restarts)),
    }
}

fn restarts_said(restarts: u64) -> String {
    match restarts {
        1 => "1 restart".to_owned(),
        n => format!("{n} restarts"),
    }
}

/// Stop companion `kept` and start it again as it is kept.
async fn restart(client: &agent_client::Client, kept: &serde_json::Value) -> Result<(), String> {
    let name = kept["name"].as_str().unwrap_or_default();
    let mut add = serde_json::json!({"name": name, "command": kept["command"],
        "args": kept["args"], "env": kept["env"]});
    if !kept["cwd"].is_null() {
        add["cwd"] = kept["cwd"].clone();
    }
    for asked in [
        serde_json::json!({"remove": name}),
        serde_json::json!({"add": add}),
    ] {
        (client.request("companions", asked).await).map_err(|e| format!("{name}: {e}"))?;
    }
    Ok(())
}

fn tilde(path: &Path) -> String {
    crate::tilde(&path.to_string_lossy())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
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
                "Another sonny upgrade{holder} is running. Please try again in a moment."
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
/// started (`sonny upgrade`, `brew upgrade`, install.sh, a rebuild): its
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
    fn stable_upgrade_requires_a_successful_empty_listing_to_report_current() {
        use std::os::unix::fs::PermissionsExt;
        let prefix = std::env::temp_dir().join(format!("sonny-brew-list-{}", std::process::id()));
        let bin = prefix.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let brew_path = bin.join("brew");
        for (code, listing, current) in [
            (0, r#"{"formulae":[]}"#, true),
            (1, r#"{"formulae":[]}"#, false),
            (0, "{}", false),
            (1, r#"{"formulae":[{"name":"agent"}]}"#, false),
        ] {
            std::fs::write(&brew_path, format!(
                "#!/bin/sh\ncase \"$1\" in\nupdate|upgrade) exit 0;;\noutdated) echo '{listing}'; exit {code};;\nesac\nexit 2\n"
            )).unwrap();
            std::fs::set_permissions(&brew_path, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(
                brew(&prefix).unwrap().is_none(),
                current,
                "{code} {listing}"
            );
        }
        std::fs::remove_dir_all(prefix).unwrap();
    }

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
        assert_eq!(identity(&link), identity(&new));
        // A file put in place of the old one, as a reinstall does, is
        // another program at the same path.
        // Held open, as a running program is, so its inode is not reused.
        let before = identity(&old);
        let _running = std::fs::File::open(&old).unwrap();
        std::fs::remove_file(&old).unwrap();
        std::fs::write(&old, "reinstalled").unwrap();
        assert!(identity(&old).is_some() && identity(&old) != before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A stand-in daemon keeping companions as the daemon does: one added
    /// with a new spec starts again, `run` saying how each run goes; the
    /// same spec again changes nothing. The removes it was asked for.
    async fn daemon_keeping(
        kept: Vec<serde_json::Value>,
        run: fn(&str) -> serde_json::Value,
    ) -> (
        std::sync::Arc<agent_client::Client>,
        std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        use serde_json::json;
        let kept = std::sync::Arc::new(std::sync::Mutex::new(kept));
        let removed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (state, gone) = (kept.clone(), removed.clone());
        let spec = |c: &serde_json::Value| {
            [&c["command"], &c["args"], &c["env"], &c["cwd"]].map(Clone::clone)
        };
        let client = crate::tests::daemon_answering(move |asked| {
            let mut kept = state.lock().unwrap();
            if let Some(name) = asked["remove"].as_str() {
                gone.lock().unwrap().push(name.to_owned());
                kept.retain(|c| c["name"] != name);
                return json!({"name": name, "removed": true});
            }
            if let Some(add) = asked["add"].as_object() {
                let mut add = serde_json::Value::Object(add.clone());
                if add.get("cwd").is_none() {
                    add["cwd"] = serde_json::Value::Null;
                }
                let name = add["name"].as_str().unwrap().to_owned();
                if kept
                    .iter()
                    .any(|c| c["name"] == *name && spec(c) == spec(&add))
                {
                    return json!({"changed": false});
                }
                kept.retain(|c| c["name"] != *name);
                let mut shown = run(&name);
                for key in ["name", "command", "args", "env", "cwd"] {
                    shown[key] = add[key].clone();
                }
                kept.push(shown);
                return json!({"changed": true});
            }
            json!({"companions": *kept})
        })
        .await;
        (client, kept, removed)
    }

    /// A run that started just now and keeps running.
    fn runs(_: &str) -> serde_json::Value {
        serde_json::json!({"running": true, "pid": 42, "started_ms": now_ms(),
            "restarts": 0, "last_exit": null})
    }

    /// The tool host exits each time it starts; the rest run.
    fn tool_host_exits(name: &str) -> serde_json::Value {
        match name == crate::TOOL_HOST {
            true => serde_json::json!({"running": false, "pid": null, "started_ms": now_ms(),
                "restarts": 3, "last_exit": {"code": 1, "signal": null}}),
            false => runs(name),
        }
    }

    /// Companion `name` as this build keeps it, up for an hour.
    fn this_build(name: &str, program: &str, verb: &str) -> serde_json::Value {
        let me = crate::named::me().unwrap();
        let mut env = serde_json::json!({"SONNY_BUILD": crate::lookout::identity(),
            crate::named::PROGRAM: me});
        if name == crate::REVIEWER {
            env["AGENT_MODEL"] = serde_json::json!("openai/gpt-6");
        }
        serde_json::json!({"name": name, "command": crate::named::named(&me, program),
            "args": [verb], "env": env, "cwd": null, "running": true, "pid": 7,
            "started_ms": now_ms() - 3_600_000, "restarts": 0, "last_exit": null})
    }

    fn all_this_build() -> Vec<serde_json::Value> {
        vec![
            this_build(crate::LOOKOUT, crate::named::LOOKOUT, "lookout"),
            this_build(
                crate::TOOL_HOST,
                crate::named::TOOL_HOST,
                crate::tool_host::NAME,
            ),
            this_build(crate::REVIEWER, crate::named::REVIEWER, "reviewer"),
        ]
    }

    const QUICK: Watch = Watch {
        settled: std::time::Duration::from_millis(100),
        until: std::time::Duration::from_secs(3),
        every: std::time::Duration::from_millis(20),
    };

    #[tokio::test]
    async fn companions_of_another_build_are_replaced_and_said_so() {
        let older = |c: serde_json::Value| {
            let mut c = c;
            c["env"]["SONNY_BUILD"] = serde_json::json!("0.10.1 1");
            c
        };
        let (client, kept, removed) =
            daemon_keeping(all_this_build().into_iter().map(older).collect(), runs).await;
        let checked = check_companions(&client, Path::new("/s/state.sqlite"), &QUICK)
            .await
            .unwrap();
        let was = || Checked::Replaced("0.10.1".to_owned());
        assert_eq!(
            checked,
            [
                (crate::LOOKOUT, was()),
                (crate::TOOL_HOST, was()),
                (crate::REVIEWER, was())
            ]
        );
        assert_eq!(
            said(crate::LOOKOUT, &was()),
            "Companion sonny-lookout: replaced (was 0.10.1)"
        );
        // Each on this build, the reviewer with the model it had.
        for c in kept.lock().unwrap().iter() {
            assert_eq!(
                c["env"]["SONNY_BUILD"],
                serde_json::json!(crate::lookout::identity())
            );
        }
        assert_eq!(
            kept.lock().unwrap()[2]["env"]["AGENT_MODEL"],
            "openai/gpt-6"
        );
        assert!(removed.lock().unwrap().is_empty());
        assert_eq!(still_failing(&[]), Ok(()));
    }

    #[tokio::test]
    async fn a_companion_that_keeps_exiting_is_failing_and_the_upgrade_exits_non_zero() {
        // On this build, but exiting each time it starts.
        let mut crashing = all_this_build();
        crashing[1] = tool_host_exits(crate::TOOL_HOST);
        for key in ["name", "command", "args", "env", "cwd"] {
            crashing[1][key] = all_this_build()[1][key].clone();
        }
        let (client, _, removed) = daemon_keeping(crashing, tool_host_exits).await;
        let checked = check_companions(&client, Path::new("/s/state.sqlite"), &QUICK)
            .await
            .unwrap();
        assert_eq!(checked[0], (crate::LOOKOUT, Checked::Ok));
        assert_eq!(checked[2], (crate::REVIEWER, Checked::Ok));
        let Checked::Failing(why) = &checked[1].1 else {
            panic!("{checked:?}");
        };
        assert_eq!(
            why,
            "exits 1, 3 restarts; its log: /s/state.sqlite.companions/sonny-tool-host.log"
        );
        // Replaced once with this build: stopped and started again.
        assert_eq!(*removed.lock().unwrap(), [crate::TOOL_HOST]);
        assert_eq!(
            still_failing(&[crate::TOOL_HOST]),
            Err(format!("sonny-tool-host is still failing on {VERSION}"))
        );
    }

    #[tokio::test]
    async fn a_restarted_companion_is_watched_by_its_new_run() {
        // This build's, running for 10 s after two restarts: restarted, and
        // the new run, which exits, is what is watched.
        let mut restarting = all_this_build();
        restarting[1]["restarts"] = serde_json::json!(2);
        restarting[1]["started_ms"] = serde_json::json!(now_ms() - 10_000);
        let (client, _, removed) = daemon_keeping(restarting, tool_host_exits).await;
        let checked = check_companions(&client, Path::new("/s/state.sqlite"), &QUICK)
            .await
            .unwrap();
        assert_eq!(*removed.lock().unwrap(), [crate::TOOL_HOST]);
        assert!(
            matches!(&checked[1].1, Checked::Failing(why) if why.starts_with("exits 1, 3 restarts")),
            "{checked:?}"
        );
    }

    #[tokio::test]
    async fn companions_all_current_are_ok_and_nothing_restarts() {
        let (client, kept, removed) = daemon_keeping(all_this_build(), runs).await;
        let started = std::time::Instant::now();
        let checked = check_companions(&client, Path::new("/s/state.sqlite"), &Watch::UPGRADE)
            .await
            .unwrap();
        assert_eq!(
            checked,
            [
                (crate::LOOKOUT, Checked::Ok),
                (crate::TOOL_HOST, Checked::Ok),
                (crate::REVIEWER, Checked::Ok)
            ]
        );
        assert_eq!(
            said(crate::LOOKOUT, &Checked::Ok),
            "Companion sonny-lookout: ok"
        );
        // Up long enough already: no wait, and the same runs.
        assert!(started.elapsed() < Watch::UPGRADE.settled);
        assert!(removed.lock().unwrap().is_empty());
        assert!(kept.lock().unwrap().iter().all(|c| c["pid"] == 7));
    }

    #[tokio::test]
    async fn a_missing_companion_is_started_and_no_reviewer_is_made() {
        let (client, kept, _) = daemon_keeping(Vec::new(), runs).await;
        let checked = check_companions(&client, Path::new("/s/state.sqlite"), &QUICK)
            .await
            .unwrap();
        assert_eq!(
            checked,
            [
                (crate::LOOKOUT, Checked::Started),
                (crate::TOOL_HOST, Checked::Started)
            ]
        );
        assert_eq!(kept.lock().unwrap().len(), 2);
    }
}
