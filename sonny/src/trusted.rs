//! The programs Sonny's tools run outside the sandbox, agent-browser and
//! agent-desktop, as copies no agent can swap. Each is found on `PATH` as
//! before, its native program where npm installs one. Before it is first
//! run, and again whenever it changes, it is admitted: refused while it,
//! or the folder its copy goes in, is in a folder some agent of this store
//! writes, else copied into `~/.agent/sonny/programs/NAME/`. The tool runs
//! only that copy, with `~/.agent/run/sonny/` as its temporary folder,
//! checked the same way. An agent writes a path as the daemon's sandbox
//! decides: the deepest of its folders that holds the path, a folder a
//! link names by where it leads, but not inside what the daemon keeps
//! read-only in every folder that holds it, `~/.agent` and the walled
//! `~/.agent/run` (as the agent CLI starts it), so a lead opened in HOME
//! counts for neither. A copy linked to from elsewhere is copied again
//! before it runs. Then: what a program leaves running there, as
//! agent-desktop leaves the host its commands go through and agent-browser
//! its sessions, was started by Sonny from the copy, is reached by no
//! other caller, and, in the folder the daemon walls off, by no sandboxed
//! command, whose network would otherwise reach it. So an agent that can write where the program is
//! installed never has it run, whichever agent calls the tool, and a swap
//! after it was admitted changes nothing that runs until it is admitted
//! again. An agent with no sandbox is not counted: it runs as the person
//! already, and gains nothing by swapping a program.
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The native program npm's launcher at `PATH`'s first `program` would
/// run beside it, so no node starts for each command, else that program.
/// The first in a folder named from the root, so one an agent puts in a
/// folder of its own, or `.`, is never found.
pub fn native(program: &str, path: &OsStr) -> PathBuf {
    let found = std::env::split_paths(path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(program))
        .find(|path| path.is_file());
    let Some(found) = found else {
        // None found runs nothing: an absolute path that is not there.
        return PathBuf::from("/nonexistent").join(program);
    };
    let real = std::fs::canonicalize(&found).unwrap_or(found);
    let native = real.with_file_name(format!("{program}-{}", platform()));
    match native.is_file() {
        true => native,
        false => real,
    }
}

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

/// What changes when a file is written, replaced, or linked to from
/// another folder, where whoever writes that folder could write it.
#[derive(Clone, Debug, PartialEq)]
struct Stamp(u64, u64, u64, u64, i64, i64, i64, i64);

fn stamp(path: &Path) -> Option<Stamp> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(path).ok()?;
    Some(Stamp(
        m.dev(),
        m.ino(),
        m.nlink(),
        m.size(),
        m.mtime(),
        m.mtime_nsec(),
        m.ctime(),
        m.ctime_nsec(),
    ))
}

fn stamps(paths: &[PathBuf]) -> Option<Vec<Stamp>> {
    paths.iter().map(|path| stamp(path)).collect()
}

/// A folder an agent writes: the agent, and the folder.
pub type Writer = (String, PathBuf);

/// One program as last admitted: its files and their copies, as they were.
struct Admitted {
    files: Vec<PathBuf>,
    origin: Vec<Stamp>,
    copies: Vec<PathBuf>,
    copied: Vec<Stamp>,
}

/// The programs admitted, by name, each copied into `dir/NAME`, the
/// temporary folder they run with, and what the daemon keeps read-only in
/// every agent's folders that hold it.
pub struct Pins {
    dir: PathBuf,
    run: PathBuf,
    kept: Vec<PathBuf>,
    admitted: tokio::sync::Mutex<HashMap<String, Admitted>>,
}

impl Pins {
    pub fn new(dir: PathBuf, run: PathBuf, kept: Vec<PathBuf>) -> Pins {
        Pins {
            dir,
            run,
            kept: kept.iter().map(|path| real(path)).collect(),
            admitted: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// The copy of `program`, as found at `origin`, to run: admitted
    /// again when it or its copy changed, against the folders `writers`
    /// gives, which are asked for only then.
    pub async fn program<F, Fut>(
        &self,
        program: &str,
        origin: &Path,
        writers: F,
    ) -> Result<PathBuf, String>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Vec<Writer>, String>>,
    {
        let mut admitted = self.admitted.lock().await;
        let files = files(program, origin);
        if let Some(was) = admitted.get(program)
            && was.files == files
            && stamps(&files).as_ref() == Some(&was.origin)
            && stamps(&was.copies).as_ref() == Some(&was.copied)
        {
            return Ok(was.copies[0].clone());
        }
        admitted.remove(program);
        if !origin.is_file() {
            return Err(format!("{} is not there", origin.display()));
        }
        let into = self.dir.join(program);
        let writers = writers().await?;
        let mut checked: Vec<&Path> = files.iter().map(PathBuf::as_path).collect();
        checked.extend([into.as_path(), self.run.as_path()]);
        for path in checked {
            if let Some((agent, folder)) = written(path, &writers, &self.kept) {
                return Err(format!(
                    "{} is in {}, which the agent {agent} writes from its \
                     sandbox, so it could put a program of its own there that runs as you. Set \
                     {agent} to Read only with /permissions, or give it a folder that does not \
                     hold {}",
                    path.display(),
                    folder.display(),
                    path.display(),
                ));
            }
        }
        let origin_stamps = stamps(&files).ok_or("it changed while it was checked")?;
        let copies = copy(&files, &into).map_err(|e| format!("copying it: {e}"))?;
        // What was copied is what was checked, not a swap in between.
        if stamps(&files).as_ref() != Some(&origin_stamps) {
            return Err(format!("{} changed while it was copied", origin.display()));
        }
        let copied = stamps(&copies).ok_or("its copy is gone")?;
        let path = copies[0].clone();
        admitted.insert(
            program.to_owned(),
            Admitted {
                files,
                origin: origin_stamps,
                copies,
                copied,
            },
        );
        Ok(path)
    }
}

/// A writer that may write `path`'s real path, if one may, as the
/// daemon's sandbox decides: for each agent, the deepest of its folders
/// that holds it, unless one of `kept`, held read-only in every folder
/// that holds it, holds the path too.
fn written<'a>(path: &Path, writers: &'a [Writer], kept: &[PathBuf]) -> Option<&'a Writer> {
    let real = real(path);
    let mut deepest: Vec<(&Writer, PathBuf)> = Vec::new();
    for writer in writers {
        let folder = std::fs::canonicalize(&writer.1).unwrap_or_else(|_| writer.1.clone());
        if !real.starts_with(&folder) {
            continue;
        }
        match deepest.iter_mut().find(|(was, _)| was.0 == writer.0) {
            Some(was) if was.1.starts_with(&folder) => {}
            Some(was) => *was = (writer, folder),
            None => deepest.push((writer, folder)),
        }
    }
    deepest
        .into_iter()
        .find(|(_, folder)| {
            !(kept.iter()).any(|kept| real.starts_with(kept) && kept.starts_with(folder))
        })
        .map(|(writer, _)| writer)
}

/// `path` with its symbolic links resolved, as far as it exists.
fn real(path: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut at = path.to_path_buf();
    loop {
        if let Ok(real) = std::fs::canonicalize(&at) {
            return rest.iter().rev().fold(real, |path, part| path.join(part));
        }
        match (at.file_name().map(|name| name.to_owned()), at.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                at = parent.to_path_buf();
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// Copy `files` into `into` by their names, each written beside its place
/// and moved there whole, so a copy running still runs as it was.
fn copy(files: &[PathBuf], into: &Path) -> std::io::Result<Vec<PathBuf>> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(into)?;
    std::fs::set_permissions(into, std::fs::Permissions::from_mode(0o700))?;
    let mut copies = Vec::new();
    for file in files {
        let name = file.file_name().ok_or(std::io::ErrorKind::InvalidInput)?;
        let copy = into.join(name);
        let partial = into.join(format!(
            ".{}.{}.partial",
            name.to_string_lossy(),
            std::process::id()
        ));
        std::fs::copy(file, &partial)?;
        std::fs::set_permissions(&partial, std::fs::Permissions::from_mode(0o700))?;
        std::fs::rename(&partial, &copy)?;
        copies.push(copy);
    }
    Ok(copies)
}

/// Sonny's own folder, `name` in `~/.agent/sonny`.
fn dir(name: &str) -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(".agent/sonny")
        .join(name)
}

/// The temporary folder `pins`'s programs run with, there and private:
/// agent-desktop starts no host in a folder others could open.
fn private(pins: &Pins) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(&pins.run)?;
    std::fs::set_permissions(&pins.run, std::fs::Permissions::from_mode(0o700))?;
    Ok(pins.run.clone())
}

/// The folders the agents of `client`'s store write, from their sandboxes.
pub async fn writers(client: &agent_client::Client) -> Result<Vec<Writer>, String> {
    let mut found = Vec::new();
    let mut after = serde_json::Value::Null;
    loop {
        let page = client
            .request("bots", serde_json::json!({"after": after, "limit": 256}))
            .await
            .map_err(|e| format!("listing the agents: {e}"))?;
        for bot in page["bots"].as_array().into_iter().flatten() {
            let inspected = client
                .request(
                    "inspect",
                    serde_json::json!({"bot": bot["name"], "bot_id": bot["bot_id"]}),
                )
                .await
                .map_err(|e| format!("reading {}: {e}", bot["name"]))?;
            let name = inspected["name"].as_str().unwrap_or_default().to_owned();
            for folder in crate::browser::Reach::of(&inspected).written() {
                found.push((name.clone(), folder));
            }
        }
        after = page["next_after"].clone();
        if after.is_null() {
            return Ok(found);
        }
    }
}

/// The tool host's programs, found on its `PATH` once.
static PINS: std::sync::LazyLock<Pins> = std::sync::LazyLock::new(|| {
    let run =
        (agent_client::config::run()).unwrap_or_else(|| std::env::temp_dir().join(".agent/run"));
    // What the agent CLI starts every daemon keeping read-only
    // (`--protect-path`, `--wall`).
    let kept = [agent_client::config::home(), agent_client::config::run()];
    Pins::new(
        dir("programs"),
        run.join("sonny"),
        kept.into_iter().flatten().collect(),
    )
});

/// The temporary folder the programs run with: `TMPDIR` for each.
pub fn temporary() -> std::io::Result<PathBuf> {
    private(&PINS)
}

/// The tool host's connection to its store, whose agents the programs are
/// admitted against, wherever the call comes from: one of them, or an
/// agent of a machine linked here.
static STORE: std::sync::Mutex<Option<std::sync::Arc<agent_client::Client>>> =
    std::sync::Mutex::new(None);

/// Admit programs against the agents of `client`'s store from now on.
pub fn against(client: std::sync::Arc<agent_client::Client>) {
    *STORE.lock().unwrap_or_else(|e| e.into_inner()) = Some(client);
}

/// The copy of `program` to run, found once on this process's `PATH`, and
/// admitted against the agents of the tool host's store.
pub async fn program(program: &str) -> Result<PathBuf, String> {
    static FOUND: std::sync::LazyLock<std::sync::Mutex<HashMap<String, PathBuf>>> =
        std::sync::LazyLock::new(Default::default);
    let origin = {
        let mut found = FOUND.lock().unwrap_or_else(|e| e.into_inner());
        (found.entry(program.to_owned()))
            .or_insert_with(|| native(program, &std::env::var_os("PATH").unwrap_or_default()))
            .clone()
    };
    let client = STORE.lock().unwrap_or_else(|e| e.into_inner()).clone();
    PINS.program(program, &origin, || async move {
        match client {
            Some(client) => writers(&client).await,
            None => Err("no store's agents to check it against".to_owned()),
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> PathBuf {
        // A clock alone can give two tests started together one folder
        // (macOS reads it to the microsecond).
        static TEMPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "sonny-trusted-{}-{}-{}",
            std::process::id(),
            TEMPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(dir).unwrap()
    }

    fn install(dir: &Path, name: &str, text: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    async fn none() -> Result<Vec<Writer>, String> {
        Ok(Vec::new())
    }

    #[tokio::test]
    async fn the_copy_runs_and_a_swap_after_it_was_admitted_changes_nothing_that_runs() {
        let root = temp();
        let origin = install(&root.join("bin"), "tool", "one");
        let pins = Pins::new(root.join("programs"), root.join("run"), Vec::new());
        let copy = pins.program("tool", &origin, none).await.unwrap();
        assert_eq!(copy, root.join("programs/tool/tool"));
        assert_eq!(std::fs::read_to_string(&copy).unwrap(), "one");
        // Admitted once: the agents are not asked again while it is the same.
        let asked = pins
            .program("tool", &origin, || async {
                Err::<Vec<Writer>, _>("asked".to_owned())
            })
            .await;
        assert_eq!(asked.unwrap(), copy);
        // Replaced: what runs is still what was admitted, until it is
        // admitted again, which asks again.
        std::fs::write(root.join("bin/.swap"), "two").unwrap();
        std::fs::rename(root.join("bin/.swap"), &origin).unwrap();
        assert_eq!(std::fs::read_to_string(&copy).unwrap(), "one");
        let asked = pins
            .program("tool", &origin, || async {
                Err::<Vec<Writer>, _>("asked".to_owned())
            })
            .await;
        assert_eq!(asked.unwrap_err(), "asked");
        assert_eq!(std::fs::read_to_string(&copy).unwrap(), "one");
        // Admitted again by the person's own install: the new one runs.
        let copy = pins.program("tool", &origin, none).await.unwrap();
        assert_eq!(std::fs::read_to_string(&copy).unwrap(), "two");
        // Its copy changed under it: copied again.
        std::fs::write(&copy, "swapped").unwrap();
        let copy = pins.program("tool", &origin, none).await.unwrap();
        assert_eq!(std::fs::read_to_string(&copy).unwrap(), "two");
        // Swapped by an agent given its folder since it was admitted, as
        // while a call waits for approval: the next start asks again, and
        // refuses it.
        std::fs::write(&copy, "theirs").unwrap();
        let writers = vec![("calc.lead".to_owned(), root.join("programs"))];
        let refused = pins
            .program("tool", &origin, || async { Ok(writers) })
            .await
            .unwrap_err();
        assert!(
            refused.contains("which the agent calc.lead writes"),
            "{refused}"
        );
    }

    #[tokio::test]
    async fn a_program_in_a_folder_any_agent_writes_is_never_run() {
        let root = temp();
        let project = root.join("project");
        let origin = install(&project.join("node_modules/.bin"), "tool", "theirs");
        let pins = Pins::new(root.join("programs"), root.join("run"), Vec::new());
        // The caller's folder, or another agent's: refused either way.
        for agent in ["calc.lead", "other.lead"] {
            let writers = vec![(agent.to_owned(), project.clone())];
            let refused = pins
                .program("tool", &origin, || async { Ok(writers) })
                .await
                .unwrap_err();
            assert!(
                refused.contains(&format!("which the agent {agent} writes")),
                "{refused}"
            );
        }
        assert!(!root.join("programs/tool").exists());
        // Through a link to it, too.
        let link = root.join("linked");
        std::os::unix::fs::symlink(&project, &link).unwrap();
        let writers = vec![("calc.lead".to_owned(), link)];
        assert!(
            pins.program("tool", &origin, || async { Ok(writers) })
                .await
                .is_err()
        );
        // Nor where its copy would go.
        let elsewhere = install(&root.join("bin"), "tool", "mine");
        let writers = vec![("calc.lead".to_owned(), root.join("programs"))];
        let refused = pins
            .program("tool", &elsewhere, || async { Ok(writers) })
            .await
            .unwrap_err();
        assert!(refused.contains("programs/tool"), "{refused}");
        // Nor when the folder they run with is one an agent writes, where
        // it could leave a host of its own for them to talk to.
        let writers = vec![("calc.lead".to_owned(), root.join("run"))];
        let refused = pins
            .program("tool", &elsewhere, || async { Ok(writers) })
            .await
            .unwrap_err();
        assert!(refused.contains("run"), "{refused}");
        assert!(!root.join("programs/tool").exists());
    }

    #[tokio::test]
    async fn a_lead_opened_in_home_does_not_keep_the_programs_from_running() {
        // A HOME whose `.agent`, and the walled `.agent/run` in it, the
        // daemon keeps read-only for an agent whose folder holds them.
        let home = temp();
        let elsewhere = temp();
        let origin = install(&elsewhere.join("bin"), "tool", "mine");
        let agent = home.join(".agent");
        let kept = vec![agent.clone(), agent.join("run")];
        let pins = Pins::new(
            agent.join("sonny/programs"),
            agent.join("run/sonny"),
            kept.clone(),
        );
        let opened = vec![("shop.lead".to_owned(), home.clone())];
        let copy = pins
            .program("tool", &origin, || async { Ok(opened) })
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&copy).unwrap(), "mine");
        // Given a folder in it by name, which the daemon lets it write:
        // refused, saying why and what to change.
        let pins = Pins::new(
            agent.join("sonny/programs"),
            agent.join("run/sonny"),
            kept.clone(),
        );
        let given = vec![
            ("shop.lead".to_owned(), home.clone()),
            ("shop.lead".to_owned(), agent.join("sonny")),
        ];
        let refused = pins
            .program("tool", &origin, || async { Ok(given) })
            .await
            .unwrap_err();
        assert!(
            refused.contains("could put a program of its own there"),
            "{refused}"
        );
        assert!(
            refused.contains("Set shop.lead to Read only with /permissions"),
            "{refused}"
        );
        // Its own folder still holds what is not kept: a program installed
        // there is refused.
        let theirs = install(&home.join("bin"), "tool", "theirs");
        let opened = vec![("shop.lead".to_owned(), home.clone())];
        assert!(
            pins.program("tool", &theirs, || async { Ok(opened) })
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_copy_linked_to_from_another_folder_is_copied_again() {
        let root = temp();
        let origin = install(&root.join("bin"), "tool", "one");
        let pins = Pins::new(root.join("programs"), root.join("run"), Vec::new());
        let copy = pins.program("tool", &origin, none).await.unwrap();
        // A hard link in a folder an agent writes would let it write the
        // copy in place.
        std::fs::create_dir_all(root.join("theirs")).unwrap();
        let link = root.join("theirs/tool");
        std::fs::hard_link(&copy, &link).unwrap();
        let again = pins.program("tool", &origin, none).await.unwrap();
        std::fs::write(&link, "swapped").unwrap();
        assert_eq!(std::fs::read_to_string(&again).unwrap(), "one");
        use std::os::unix::fs::MetadataExt;
        assert_eq!(std::fs::metadata(&again).unwrap().nlink(), 1);
    }

    #[test]
    fn the_programs_run_with_a_temporary_folder_only_this_user_opens() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp();
        let pins = Pins::new(root.join("programs"), root.join("run"), Vec::new());
        assert_eq!(private(&pins).unwrap(), root.join("run"));
        // Loosened since, it is made private again before the next run.
        std::fs::set_permissions(root.join("run"), std::fs::Permissions::from_mode(0o755)).unwrap();
        private(&pins).unwrap();
        let mode = std::fs::metadata(root.join("run"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[tokio::test]
    async fn a_native_program_comes_with_the_others_of_its_package_for_this_platform() {
        let root = temp();
        let bin = root.join("node_modules/agent-desktop/bin");
        let platform = platform();
        let origin = install(&bin, &format!("agent-desktop-{platform}"), "main");
        install(
            &bin,
            &format!("agent-desktop-macos-helper-{platform}"),
            "helper",
        );
        install(&bin, "agent-desktop-win32-x64.exe", "other");
        install(&bin, "agent-desktop", "launcher");
        let pins = Pins::new(root.join("programs"), root.join("run"), Vec::new());
        let copy = pins.program("agent-desktop", &origin, none).await.unwrap();
        let mut copied: Vec<String> = std::fs::read_dir(copy.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        copied.sort();
        assert_eq!(
            copied,
            [
                format!("agent-desktop-{platform}"),
                format!("agent-desktop-macos-helper-{platform}")
            ]
        );
    }

    #[test]
    fn the_native_program_runs_beside_npm_s_launcher() {
        let root = temp();
        let package = root.join("lib/node_modules/tool/bin");
        install(&package, "tool", "#!/usr/bin/env node");
        let wanted = install(&package, &format!("tool-{}", platform()), "");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::os::unix::fs::symlink(package.join("tool"), root.join("bin/tool")).unwrap();
        let path = std::env::join_paths(["bin".into(), root.join("bin")]).unwrap();
        // `bin` is relative: a folder of whoever runs it, so skipped.
        assert_eq!(native("tool", &path), wanted);
        assert!(native("tool", OsStr::new("bin")).is_absolute());
    }
}
