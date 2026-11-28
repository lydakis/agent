//! The programs Sonny's tools run outside the sandbox, agent-browser and
//! agent-desktop: Sonny's own copies, of the versions its tools were
//! written against, as Codex runs the binary its npm package ships rather
//! than one found on `PATH`. `sonny setup` and `sonny upgrade` install each
//! from its npm release, checked against the integrity pinned here, into
//! `~/.agent/sonny/programs/NAME/`, and the tools run only that copy, with
//! `~/.agent/run/sonny/` as its temporary folder. The daemon keeps
//! `~/.agent` read-only to every sandbox and walls off `~/.agent/run`, so
//! no agent can swap a program that runs as the person, and none is looked
//! for anywhere an agent writes. What a program leaves running in that
//! temporary folder, as agent-desktop leaves the host its commands go
//! through and agent-browser its sessions, was started by Sonny from its
//! copy and is reached by no sandboxed command.
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// A program Sonny installs: its npm package, the version its tools list
/// the commands of (`sonny/upstream/NAME.txt`), and that release's npm
/// integrity, which its tarball must match.
pub struct Pin {
    pub name: &'static str,
    pub version: &'static str,
    integrity: &'static str,
}

/// The programs, pinned. Moving one is moving its list in
/// `sonny/upstream/` with it.
pub const PINS: [Pin; 2] = [
    // Its own tarball holds every platform's program up to 0.42. From
    // 0.43.0 each platform's is in a package of its own,
    // `@agent-browser/cli-PLATFORM` (`-gnu` on Linux), as `bin/agent-browser`:
    // a pin past 0.42 is installed from that package, with an integrity
    // pinned for each platform.
    Pin {
        name: "agent-browser",
        version: "0.41.1",
        integrity: "sha512-9DhCeIhcpxu3Mj+f25Bolk6N+BT5SpOttRSZw0Ytz5MZ8zpKeVp2bv9Zxo1gajlvZE3l//rOoiRdq+qTYHBg1Q==",
    },
    Pin {
        name: "agent-desktop",
        version: "1.0.2",
        integrity: "sha512-k2Qi3f0y1U60YaJOw1dob3EO55P/DuH4HK3WKdI9sMZLjVxpGZAzuG50rIWuIqS0JIVy07g+0pp9Of2JDjTX0w==",
    },
];

/// The file in a program's folder naming the version installed there.
const VERSION: &str = "VERSION";

/// How npm names a native program's platform: `darwin-arm64`.
fn platform() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        os => os,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        arch => arch,
    };
    format!("{os}-{arch}")
}

/// What runs: `origin`, and beside a native program the others of its
/// package it starts, for this platform, by name from its folder, as
/// agent-desktop starts its helper.
fn files(program: &str, origin: &Path) -> Vec<PathBuf> {
    let mut files = vec![origin.to_path_buf()];
    let suffix = format!("-{}", platform());
    let native = (origin.file_name().and_then(OsStr::to_str))
        .is_some_and(|name| name == format!("{program}{suffix}"));
    if let (true, Some(dir)) = (native, origin.parent()) {
        let mut beside: Vec<PathBuf> = (std::fs::read_dir(dir).into_iter().flatten().flatten())
            .map(|entry| entry.path())
            .filter(|path| path != origin && path.is_file())
            .filter(|path| {
                (path.file_name().and_then(OsStr::to_str)).is_some_and(|name| {
                    name.starts_with(&format!("{program}-")) && name.ends_with(&suffix)
                })
            })
            .collect();
        beside.sort();
        files.extend(beside);
    }
    files
}

/// Where the programs are installed, one folder each: a folder the daemon
/// keeps from every sandbox ([`agent_client::config::programs`]).
fn programs() -> PathBuf {
    agent_client::config::programs()
        .unwrap_or_else(|| std::env::temp_dir().join(".agent/sonny/programs"))
}

/// The program `name` runs as, in `programs`: its native program for
/// this platform.
fn at(programs: &Path, name: &str) -> PathBuf {
    programs.join(name).join(format!("{name}-{}", platform()))
}

/// Whether Sonny's copy of `name` is installed.
pub fn here(name: &str) -> bool {
    at(&programs(), name).is_file()
}

/// Sonny's copy of `name` to run, or why there is none.
pub fn program(name: &str) -> Result<PathBuf, String> {
    let path = at(&programs(), name);
    match path.is_file() {
        true => Ok(path),
        false => Err(format!(
            "{name} is not installed: `sonny upgrade` or `sonny setup` installs it in {}",
            crate::tilde(&programs().join(name).display().to_string())
        )),
    }
}

/// The version of `name` installed in `programs`, if any.
fn installed(programs: &Path, name: &str) -> Option<String> {
    at(programs, name).is_file().then_some(())?;
    let text = std::fs::read_to_string(programs.join(name).join(VERSION)).ok()?;
    Some(text.trim().to_owned())
}

/// Whether this machine wants `pin`: agent-browser always, agent-desktop
/// where there is a desktop to use.
fn wanted(pin: &Pin) -> bool {
    pin.name != "agent-desktop" || crate::setup::desktop_shows()
}

/// Bring each program this machine wants to its pinned version, saying
/// what it does: the errors, one a program, of those it could not.
pub fn bring_up(say: &dyn Fn(&str)) -> Vec<String> {
    let programs = programs();
    (PINS.iter())
        .filter(|pin| wanted(pin))
        .filter(|pin| installed(&programs, pin.name).as_deref() != Some(pin.version))
        .filter_map(|pin| {
            say(&format!("Installing {} {}", pin.name, pin.version));
            let url = url(pin);
            install(&programs, pin, &|archive: &Path| download(&url, archive))
                .err()
                .map(|error| format!("{} {}: {error}", pin.name, pin.version))
        })
        .collect()
}

/// Where npm serves `pin`'s tarball.
fn url(pin: &Pin) -> String {
    format!(
        "https://registry.npmjs.org/{0}/-/{0}-{1}.tgz",
        pin.name, pin.version
    )
}

/// Fetch `url` to `to`.
fn download(url: &str, to: &Path) -> Result<(), String> {
    let fetched = std::process::Command::new("curl")
        .args(["-fsSL", "-o"])
        .arg(to)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("curl: {e}"))?;
    match fetched.success() {
        true => Ok(()),
        false => Err(format!("could not download {url}")),
    }
}

/// Install `pin` in `programs`, its tarball fetched by `fetch`: checked
/// against its integrity, its native programs for this platform unpacked
/// beside one another, and the folder moved into place whole, so a copy
/// running keeps running as it was.
fn install(
    programs: &Path,
    pin: &Pin,
    fetch: &dyn Fn(&Path) -> Result<(), String>,
) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(programs).map_err(io(programs))?;
    std::fs::set_permissions(programs, std::fs::Permissions::from_mode(0o700))
        .map_err(io(programs))?;
    let work = programs.join(format!(".{}.{}", pin.name, std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).map_err(io(&work))?;
    let result = unpack(programs, pin, fetch, &work);
    let _ = std::fs::remove_dir_all(&work);
    result
}

fn unpack(
    programs: &Path,
    pin: &Pin,
    fetch: &dyn Fn(&Path) -> Result<(), String>,
    work: &Path,
) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let archive = work.join("package.tgz");
    fetch(&archive)?;
    if integrity(&archive)? != pin.integrity {
        return Err("its tarball does not match the integrity Sonny pins".into());
    }
    let unpacked = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(work)
        .arg("package/bin")
        .stdin(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("tar: {e}"))?;
    let origin = work
        .join("package/bin")
        .join(format!("{}-{}", pin.name, platform()));
    if !unpacked.success() || !origin.is_file() {
        return Err(format!(
            "its tarball has no {} for {}",
            pin.name,
            platform()
        ));
    }
    let made = work.join(pin.name);
    std::fs::create_dir_all(&made).map_err(io(&made))?;
    std::fs::set_permissions(&made, std::fs::Permissions::from_mode(0o700)).map_err(io(&made))?;
    for file in files(pin.name, &origin) {
        let to = made.join(file.file_name().unwrap_or_default());
        std::fs::rename(&file, &to).map_err(io(&to))?;
        std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o700)).map_err(io(&to))?;
    }
    let version = made.join(VERSION);
    std::fs::write(&version, format!("{}\n", pin.version)).map_err(io(&version))?;
    let into = programs.join(pin.name);
    let old = work.join("old");
    if into.exists() {
        std::fs::rename(&into, &old).map_err(io(&into))?;
    }
    std::fs::rename(&made, &into).map_err(io(&into))
}

/// An error of the file at `path`, saying which.
fn io(path: &Path) -> impl Fn(std::io::Error) -> String + '_ {
    move |e| format!("{}: {e}", path.display())
}

/// npm's integrity of the file at `path`: `sha512-` and its SHA-512 in
/// base64.
fn integrity(path: &Path) -> Result<String, String> {
    use base64::Engine;
    use sha2::Digest;
    let mut file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut hasher = sha2::Sha512::new();
    std::io::copy(&mut file, &mut hasher).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(format!(
        "sha512-{}",
        base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
    ))
}

/// The temporary folder the programs run with, `run`, there and private:
/// agent-desktop starts no host in a folder others could open.
fn private(run: &Path) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(run)?;
    std::fs::set_permissions(run, std::fs::Permissions::from_mode(0o700))?;
    Ok(run.to_path_buf())
}

/// The temporary folder the programs run with: `TMPDIR` for each.
pub fn temporary() -> std::io::Result<PathBuf> {
    let run =
        (agent_client::config::run()).unwrap_or_else(|| std::env::temp_dir().join(".agent/run"));
    private(&run.join("sonny"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> PathBuf {
        static TEMPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "sonny-trusted-{}-{}",
            std::process::id(),
            TEMPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(dir).unwrap()
    }

    /// An npm tarball holding `files` under `package/bin`, and its
    /// integrity.
    fn tarball(root: &Path, files: &[(&str, &str)]) -> (PathBuf, String) {
        let bin = root.join("made/package/bin");
        std::fs::create_dir_all(&bin).unwrap();
        for (name, text) in files {
            std::fs::write(bin.join(name), text).unwrap();
        }
        let archive = root.join("package.tgz");
        let made = std::process::Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(root.join("made"))
            .arg("package")
            .status()
            .unwrap();
        assert!(made.success());
        let sum = integrity(&archive).unwrap();
        (archive, sum)
    }

    fn pin(integrity: &str) -> Pin {
        Pin {
            name: "tool",
            version: "1.2.3",
            // Leaked: a test's pin lives as long as the pins it stands for.
            integrity: Box::leak(integrity.to_owned().into_boxed_str()),
        }
    }

    #[test]
    fn a_program_is_installed_from_its_checked_tarball_with_its_platform_s_files() {
        let root = temp();
        let platform = platform();
        let (archive, sum) = tarball(
            &root,
            &[
                (&format!("tool-{platform}"), "main"),
                (&format!("tool-helper-{platform}"), "helper"),
                ("tool-win32-x64.exe", "other"),
                ("tool", "#!/usr/bin/env node"),
            ],
        );
        let programs = root.join("programs");
        let fetch = |to: &Path| {
            std::fs::copy(&archive, to)
                .map(|_| ())
                .map_err(|e| e.to_string())
        };
        install(&programs, &pin(&sum), &fetch).unwrap();
        let program = at(&programs, "tool");
        assert_eq!(std::fs::read_to_string(&program).unwrap(), "main");
        assert_eq!(installed(&programs, "tool").as_deref(), Some("1.2.3"));
        let mut copied: Vec<String> = std::fs::read_dir(programs.join("tool"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        copied.sort();
        let mut wanted = [
            VERSION.to_owned(),
            format!("tool-helper-{platform}"),
            format!("tool-{platform}"),
        ];
        wanted.sort();
        assert_eq!(copied, wanted);
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!((mode(&programs), mode(&program)), (0o700, 0o700));
        // Installed again over itself: one folder, nothing left beside it.
        install(&programs, &pin(&sum), &fetch).unwrap();
        let left: Vec<_> = std::fs::read_dir(&programs).unwrap().collect();
        assert_eq!(left.len(), 1);
        // A tarball that is not the pinned one installs nothing, and leaves
        // what was installed.
        std::fs::write(&archive, "swapped").unwrap();
        let refused = install(&programs, &pin(&sum), &fetch).unwrap_err();
        assert!(refused.contains("does not match"), "{refused}");
        assert_eq!(std::fs::read_to_string(&program).unwrap(), "main");
        // One without this platform's program installs nothing either.
        let other = root.join("other");
        std::fs::create_dir_all(&other).unwrap();
        let (archive, sum) = tarball(&other, &[("tool-win32-x64.exe", "other")]);
        let fetch = |to: &Path| {
            std::fs::copy(&archive, to)
                .map(|_| ())
                .map_err(|e| e.to_string())
        };
        let refused = install(&other.join("programs"), &pin(&sum), &fetch).unwrap_err();
        assert!(refused.contains("has no tool for"), "{refused}");
        assert!(!at(&other.join("programs"), "tool").exists());
    }

    #[test]
    fn the_pins_are_npm_integrities_of_the_versions_the_tools_list() {
        for pin in &PINS {
            assert!(pin.integrity.starts_with("sha512-"), "{}", pin.name);
            let listed = std::fs::read_to_string(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("upstream")
                    .join(format!("{}.txt", pin.name)),
            )
            .unwrap();
            assert_eq!(
                listed.lines().next(),
                Some(format!("# {} {}", pin.name, pin.version).as_str())
            );
        }
    }

    /// Each pin, fetched from npm and installed as `sonny setup` and
    /// `sonny upgrade` install it, runs on this machine and says its
    /// version. The Upstream workflow runs this on Linux x64 and ARM.
    #[test]
    #[ignore = "downloads the pinned releases: cargo test -p sonny pinned_programs -- --ignored"]
    fn the_pinned_programs_install_from_npm_and_run_here() {
        let programs = temp().join("programs");
        for pin in &PINS {
            let url = url(pin);
            let fetch = |archive: &Path| download(&url, archive);
            if let Err(error) = install(&programs, pin, &fetch) {
                panic!("{} {} on {}: {error}", pin.name, pin.version, platform());
            }
            let ran = std::process::Command::new(at(&programs, pin.name))
                .arg("--version")
                .stdin(std::process::Stdio::null())
                .output()
                .unwrap();
            let said = String::from_utf8_lossy(&ran.stdout);
            assert!(
                ran.status.success(),
                "{} on {}: {ran:?}",
                pin.name,
                platform()
            );
            assert!(
                said.contains(pin.version),
                "{} on {}: {said}",
                pin.name,
                platform()
            );
        }
    }

    #[test]
    fn the_programs_run_with_a_temporary_folder_only_this_user_opens() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp();
        assert_eq!(private(&root.join("run")).unwrap(), root.join("run"));
        // Loosened since, it is made private again before the next run.
        std::fs::set_permissions(root.join("run"), std::fs::Permissions::from_mode(0o755)).unwrap();
        private(&root.join("run")).unwrap();
        let mode = std::fs::metadata(root.join("run"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }
}
