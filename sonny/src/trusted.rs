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
/// the commands of (`sonny/upstream/NAME.txt`), that release's npm
/// integrity, which its tarball must match, and where its native program
/// for each platform is.
pub struct Pin {
    pub name: &'static str,
    pub version: &'static str,
    integrity: &'static str,
    natives: Natives,
}

/// Where a pin's native programs are.
enum Natives {
    /// In its own package, each platform's as `bin/NAME-PLATFORM`.
    Own,
    /// Each platform's in a package of its own, as `bin/NAME`: the
    /// platform, the package, and its tarball's npm integrity.
    Each(&'static [(&'static str, &'static str, &'static str)]),
}

/// The programs, pinned. Moving one is moving its list in
/// `sonny/upstream/` with it.
pub const PINS: [Pin; 2] = [
    // Its own package holds its skills; each platform's program is in
    // `@agent-browser/cli-PLATFORM` (`-gnu` on Linux), the one its
    // `optionalDependencies` has npm install on that platform.
    Pin {
        name: "agent-browser",
        version: "0.45.0",
        integrity: "sha512-lzdtbRwTthLy/2FyxYBWpiI7rg6CGojoVjym1UP59hWwdTYDGSO13XwzQiEvYaInvDstHmNC5UBW7rXp6vMXMA==",
        natives: Natives::Each(&[
            (
                "darwin-arm64",
                "@agent-browser/cli-darwin-arm64",
                "sha512-5oXJso0waXDJ4JhRlf0HMFWPfzCVGCDsAmQKrN05cDluqCCUj55ImWK0XEtiFkjwdJDpHJIJV3f6OOJD+K85Bg==",
            ),
            (
                "darwin-x64",
                "@agent-browser/cli-darwin-x64",
                "sha512-ILY9co+hxsupMFvKsh+qi3+5I2QiDM44eEgf/3S8scKpP4rVX/vXEk6gScJQ9fw4qQyg37A58JLqbVSZbKUsbw==",
            ),
            (
                "linux-arm64",
                "@agent-browser/cli-linux-arm64-gnu",
                "sha512-CpYFt3QaKtnKoytgXHt/pdQjN5ZSC179YrXXhLATEGxNhyJgfuSyHGPrAtfSJ1EKDEWwhcf7Lm1yn7UKBiT8XQ==",
            ),
            (
                "linux-x64",
                "@agent-browser/cli-linux-x64-gnu",
                "sha512-3Ep+BDxFHLEqPElw/68w1OWJNs+/ucQ9RvJ47O2weqsXCLvjiDF2pmVhteXcMgbH0hUI3QK6+LG3/2ZIuC2jBQ==",
            ),
        ]),
    },
    Pin {
        name: "agent-desktop",
        version: "1.0.2",
        integrity: "sha512-k2Qi3f0y1U60YaJOw1dob3EO55P/DuH4HK3WKdI9sMZLjVxpGZAzuG50rIWuIqS0JIVy07g+0pp9Of2JDjTX0w==",
        natives: Natives::Own,
    },
];

/// What of a pin's own package is kept beside its program, as npm lays the
/// package out, when the package has it: agent-browser's skills, which it
/// serves from `skill-data/` and `skills/` beside the `bin/` it runs from.
const KEPT: [&str; 2] = ["skills", "skill-data"];

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

/// What runs: `origin`, and beside it the others of its package it
/// starts, for this platform, by name from its folder, as agent-desktop
/// starts its helper: `NAME-*-PLATFORM`.
fn files(program: &str, origin: &Path) -> Vec<PathBuf> {
    let mut files = vec![origin.to_path_buf()];
    let suffix = format!("-{}", platform());
    if let Some(dir) = origin.parent() {
        let mut beside: Vec<PathBuf> = (std::fs::read_dir(dir).into_iter().flatten().flatten())
            .map(|entry| entry.path())
            .filter(|path| path != origin && path.is_file())
            .filter(|path| {
                (path.file_name().and_then(OsStr::to_str)).is_some_and(|name| {
                    name.starts_with(&format!("{program}-"))
                        && name.ends_with(&suffix)
                        && name != format!("{program}{suffix}")
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
/// this platform, in `bin/` as in its npm package.
fn at(programs: &Path, name: &str) -> PathBuf {
    programs
        .join(name)
        .join("bin")
        .join(format!("{name}-{}", platform()))
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
            install(&programs, pin, &|package, archive| {
                download(&url(package, pin.version), archive)
            })
            .err()
            .map(|error| format!("{} {}: {error}", pin.name, pin.version))
        })
        .collect()
}

/// Where npm serves `package`'s tarball at `version`: a scoped package's
/// is named without its scope.
fn url(package: &str, version: &str) -> String {
    let base = package.rsplit('/').next().unwrap_or(package);
    format!("https://registry.npmjs.org/{package}/-/{base}-{version}.tgz")
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

/// What fetches a package's tarball, by its npm name, to a file.
type Fetch<'a> = &'a dyn Fn(&str, &Path) -> Result<(), String>;

/// Install `pin` in `programs`, each tarball fetched by `fetch` and checked
/// against its integrity: its program for this platform in `bin/` with the
/// others of its package it starts beside it, what [`KEPT`] names of its
/// own package beside `bin/`, and the folder moved into place whole, so a
/// copy running keeps running as it was.
fn install(programs: &Path, pin: &Pin, fetch: Fetch) -> Result<(), String> {
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

/// `package`'s tarball, fetched, checked against `integrity` and unpacked
/// in `into`: its folder, `into/package`.
fn fetched(package: &str, integrity: &str, fetch: Fetch, into: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(into).map_err(io(into))?;
    let archive = into.join("package.tgz");
    fetch(package, &archive)?;
    if self::integrity(&archive)? != integrity {
        return Err(format!(
            "{package}'s tarball does not match the integrity Sonny pins"
        ));
    }
    let unpacked = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(into)
        .arg("package")
        .stdin(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("tar: {e}"))?;
    match unpacked.success() {
        true => Ok(into.join("package")),
        false => Err(format!("{package}'s tarball does not unpack")),
    }
}

fn unpack(programs: &Path, pin: &Pin, fetch: Fetch, work: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let platform = platform();
    let own = fetched(pin.name, pin.integrity, fetch, &work.join("own"))?;
    let (package, origin) = match pin.natives {
        Natives::Own => (pin.name, own.join(format!("bin/{}-{platform}", pin.name))),
        Natives::Each(natives) => {
            let Some((_, package, integrity)) = natives.iter().find(|(p, ..)| *p == platform)
            else {
                return Err(format!("Sonny pins no {} for {platform}", pin.name));
            };
            let native = fetched(package, integrity, fetch, &work.join("native"))?;
            (*package, native.join("bin").join(pin.name))
        }
    };
    if !origin.is_file() {
        return Err(format!("{package} has no {} for {platform}", pin.name));
    }
    let made = work.join(pin.name);
    let bin = made.join("bin");
    std::fs::create_dir_all(&bin).map_err(io(&bin))?;
    for dir in [&made, &bin] {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(io(dir))?;
    }
    // The program by the name it runs as, `NAME-PLATFORM`, whatever its
    // package calls it; the others by their own.
    for (n, file) in files(pin.name, &origin).into_iter().enumerate() {
        let to = match n {
            0 => at(work, pin.name),
            _ => bin.join(file.file_name().unwrap_or_default()),
        };
        std::fs::rename(&file, &to).map_err(io(&to))?;
        std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o700)).map_err(io(&to))?;
    }
    for kept in KEPT {
        let from = own.join(kept);
        if from.is_dir() {
            std::fs::rename(&from, made.join(kept)).map_err(io(&from))?;
        }
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

    /// An npm tarball holding `files`, by their paths in its package, and
    /// its integrity.
    fn tarball(root: &Path, files: &[(&str, &str)]) -> (PathBuf, String) {
        let package = root.join("made/package");
        for (name, text) in files {
            let file = package.join(name);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, text).unwrap();
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

    /// A pin of `tool` 1.2.3, its own tarball of `integrity`.
    fn pin(integrity: &str, natives: Natives) -> Pin {
        Pin {
            name: "tool",
            version: "1.2.3",
            // Leaked: a test's pin lives as long as the pins it stands for.
            integrity: Box::leak(integrity.to_owned().into_boxed_str()),
            natives,
        }
    }

    /// What fetches each package's tarball from `archives`, by name.
    fn from<'a>(
        archives: &'a [(&'a str, &'a Path)],
    ) -> impl Fn(&str, &Path) -> Result<(), String> + 'a {
        move |package, to| {
            let (_, archive) = (archives.iter())
                .find(|(name, _)| *name == package)
                .ok_or(format!("no {package}"))?;
            std::fs::copy(archive, to)
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
    }

    /// What `dir` holds, every file by its path in it.
    fn listing(dir: &Path) -> Vec<String> {
        let mut found = Vec::new();
        let mut left = vec![dir.to_path_buf()];
        while let Some(at) = left.pop() {
            for entry in std::fs::read_dir(&at).unwrap() {
                let path = entry.unwrap().path();
                match path.is_dir() {
                    true => left.push(path),
                    false => found.push(path.strip_prefix(dir).unwrap().display().to_string()),
                }
            }
        }
        found.sort();
        found
    }

    #[test]
    fn a_program_is_installed_from_its_checked_tarball_with_its_platform_s_files() {
        let root = temp();
        let platform = platform();
        let (archive, sum) = tarball(
            &root,
            &[
                (&format!("bin/tool-{platform}"), "main"),
                (&format!("bin/tool-helper-{platform}"), "helper"),
                ("bin/tool-win32-x64.exe", "other"),
                ("bin/tool", "#!/usr/bin/env node"),
                ("scripts/postinstall.js", "fetch()"),
            ],
        );
        let programs = root.join("programs");
        let archives = [("tool", archive.as_path())];
        let fetch = from(&archives);
        install(&programs, &pin(&sum, Natives::Own), &fetch).unwrap();
        let program = at(&programs, "tool");
        assert_eq!(std::fs::read_to_string(&program).unwrap(), "main");
        assert_eq!(installed(&programs, "tool").as_deref(), Some("1.2.3"));
        let mut wanted = [
            VERSION.to_owned(),
            format!("bin/tool-helper-{platform}"),
            format!("bin/tool-{platform}"),
        ];
        wanted.sort();
        assert_eq!(listing(&programs.join("tool")), wanted);
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!((mode(&programs), mode(&program)), (0o700, 0o700));
        // Installed again over itself: one folder, nothing left beside it.
        install(&programs, &pin(&sum, Natives::Own), &fetch).unwrap();
        let left: Vec<_> = std::fs::read_dir(&programs).unwrap().collect();
        assert_eq!(left.len(), 1);
        // A tarball that is not the pinned one installs nothing, and leaves
        // what was installed.
        std::fs::write(&archive, "swapped").unwrap();
        let refused = install(&programs, &pin(&sum, Natives::Own), &fetch).unwrap_err();
        assert!(refused.contains("does not match"), "{refused}");
        assert_eq!(std::fs::read_to_string(&program).unwrap(), "main");
        // One without this platform's program installs nothing either.
        let other = root.join("other");
        let (archive, sum) = tarball(&other, &[("bin/tool-win32-x64.exe", "other")]);
        let archives = [("tool", archive.as_path())];
        let refused = install(
            &other.join("programs"),
            &pin(&sum, Natives::Own),
            &from(&archives),
        )
        .unwrap_err();
        assert!(refused.contains("has no tool for"), "{refused}");
        assert!(!at(&other.join("programs"), "tool").exists());
    }

    #[test]
    fn a_program_in_its_platform_s_own_package_runs_with_its_package_s_skills() {
        // agent-browser's layout: `@scope/cli-PLATFORM` holds `bin/tool`,
        // and its own package the skills it serves from beside `bin/`.
        let root = temp();
        let (own, own_sum) = tarball(
            &root.join("own"),
            &[
                ("bin/tool.js", "launcher"),
                ("skills/tool/SKILL.md", "stub"),
                ("skill-data/core/SKILL.md", "guide"),
                ("scripts/postinstall.js", "fetch()"),
            ],
        );
        let (native, native_sum) = tarball(&root.join("native"), &[("bin/tool", "native")]);
        let natives: &'static [(&str, &str, &str)] = Box::leak(Box::new([(
            Box::leak(platform().into_boxed_str()) as &str,
            "@tool/cli-here",
            Box::leak(native_sum.clone().into_boxed_str()) as &str,
        )]));
        let archives = [
            ("tool", own.as_path()),
            ("@tool/cli-here", native.as_path()),
        ];
        let programs = root.join("programs");
        install(
            &programs,
            &pin(&own_sum, Natives::Each(natives)),
            &from(&archives),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(at(&programs, "tool")).unwrap(),
            "native"
        );
        assert_eq!(
            listing(&programs.join("tool")),
            [
                VERSION.to_owned(),
                format!("bin/tool-{}", platform()),
                "skill-data/core/SKILL.md".to_owned(),
                "skills/tool/SKILL.md".to_owned(),
            ]
        );
        // The platform's package is checked as the pin's own is.
        let refused = install(
            &programs,
            &pin(
                &own_sum,
                Natives::Each(Box::leak(Box::new([(
                    natives[0].0,
                    "@tool/cli-here",
                    "sha512-x",
                )]))),
            ),
            &from(&archives),
        )
        .unwrap_err();
        assert!(
            refused.contains("@tool/cli-here's tarball does not match"),
            "{refused}"
        );
        // A platform it pins none for installs nothing.
        let refused = install(
            &programs,
            &pin(&own_sum, Natives::Each(&[])),
            &from(&archives),
        )
        .unwrap_err();
        assert!(refused.contains("pins no tool for"), "{refused}");
        assert_eq!(
            std::fs::read_to_string(at(&programs, "tool")).unwrap(),
            "native"
        );
        // A scoped package's tarball is named without its scope.
        assert_eq!(
            url("@tool/cli-here", "1.2.3"),
            "https://registry.npmjs.org/@tool/cli-here/-/cli-here-1.2.3.tgz"
        );
        assert_eq!(
            url("agent-desktop", "1.0.2"),
            "https://registry.npmjs.org/agent-desktop/-/agent-desktop-1.0.2.tgz"
        );
    }

    #[test]
    fn the_pins_are_npm_integrities_of_the_versions_the_tools_list() {
        for pin in &PINS {
            assert!(pin.integrity.starts_with("sha512-"), "{}", pin.name);
            // A package for each platform Sonny is built for: the Mac's two,
            // and Linux x86_64 and aarch64.
            if let Natives::Each(natives) = pin.natives {
                let mut platforms: Vec<&str> = natives.iter().map(|(p, ..)| *p).collect();
                platforms.sort();
                assert_eq!(
                    platforms,
                    ["darwin-arm64", "darwin-x64", "linux-arm64", "linux-x64"],
                    "{}",
                    pin.name
                );
                for (_, package, integrity) in natives {
                    assert!(integrity.starts_with("sha512-"), "{package}");
                }
            }
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
            let fetch =
                |package: &str, archive: &Path| download(&url(package, pin.version), archive);
            if let Err(error) = install(&programs, pin, &fetch) {
                panic!("{} {} on {}: {error}", pin.name, pin.version, platform());
            }
            let ran = std::process::Command::new(at(&programs, pin.name))
                .arg("--version")
                .stdin(std::process::Stdio::null())
                .output()
                .unwrap();
            // agent-browser serves its skills from those installed beside
            // it: the one Sonny installs, and the guide it points to.
            if pin.name == "agent-browser" {
                for skill in ["agent-browser", "core"] {
                    let served = std::process::Command::new(at(&programs, pin.name))
                        .args(["skills", "get", skill])
                        .env_remove("AGENT_BROWSER_SKILLS_DIR")
                        .env("HOME", programs.join("no-home"))
                        .stdin(std::process::Stdio::null())
                        .output()
                        .unwrap();
                    let text = String::from_utf8_lossy(&served.stdout);
                    assert!(
                        text.starts_with("---\n"),
                        "skill {skill} on {}: {served:?}",
                        platform()
                    );
                }
            }
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
