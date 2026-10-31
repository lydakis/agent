//! The person's home kept off the machine: `~/.agents` (memory, skills,
//! profiles) and `~/.agent/config.toml`, copied into a git repository,
//! `~/agent-home`, and pushed every ten minutes by Sonny's scheduler,
//! so a machine that dies takes at most that much with it. Keys
//! (`~/.agent/env`), logins and the daemon's store stay out.
//!
//! `sonny setup backup URL` sets it up, and restores a home the repository
//! already holds, which is how a new machine starts; `sonny home sync` is
//! what the scheduler runs, and `sonny home status` says how the last one went.
//!
//! Memory is copied without its own `.git`: a commit inside
//! `~/.agents/memory` would make `memory changed` find nothing and skip the
//! nightly cleanup.
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

/// The job the backup had of its own before Sonny's scheduler ran it,
/// which Sonny's start lets go of once.
pub(crate) const OLD_LABEL: &str = "me.lydakis.agent.backup";
/// How often the scheduler runs a sync.
pub const EVERY_SECONDS: u32 = 600;
/// Names never copied, at any depth: a repository's own, and what memory
/// keeps out of its history (a lock, read counts, an open cleanup that
/// names a commit the copy does not have).
const SKIP: [&str; 5] = [".git", ".lock", ".reads", ".cleanup", ".DS_Store"];
pub const USAGE: &str = "usage: home sync | home status\n       setup backup [URL]";

/// Where the home, its copy and what its syncs did live.
pub struct Places {
    agents: PathBuf,
    config: PathBuf,
    repo: PathBuf,
    state: PathBuf,
}

impl Places {
    pub fn home() -> Result<Self, String> {
        let home = PathBuf::from(std::env::var_os("HOME").ok_or("set HOME")?);
        Ok(Self {
            agents: home.join(".agents"),
            config: home.join(".agent/config.toml"),
            repo: home.join("agent-home"),
            state: home.join(".agent/sonny/home.json"),
        })
    }
}

/// `sonny home ARGS`: one JSON value on stdout, or a typed error on stderr.
pub fn cli(args: &[String]) -> i32 {
    let done = Places::home().and_then(|places| match args {
        [verb] if verb == "sync" => sync(&places, None),
        // Sonny's scheduler's run: git with the login shell's environment,
        // which holds the person's SSH agent and credential helpers.
        [verb, job] if verb == "sync" && job == "--job" => {
            let environment = crate::login::environment();
            sync(&places, environment.as_deref())
        }
        [verb] if verb == "status" => Ok(status(&places)),
        _ => Err(format!("usage: {USAGE}")),
    });
    match done {
        Ok(value) => {
            println!("{value}");
            0
        }
        Err(error) => {
            eprintln!("{}", error_json(&error));
            1
        }
    }
}

fn error_json(message: &str) -> Value {
    match message.split_once(": ") {
        Some((code, detail))
            if !code.is_empty() && code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') =>
        {
            json!({"error": code, "detail": detail})
        }
        _ => json!({"error": "backup_failed", "detail": message}),
    }
}

/// Set up for this setup step: a repository with a remote.
pub fn done(places: &Places) -> bool {
    origin(&places.repo, None).is_some()
}

/// Whether the scheduler runs a sync now: backup is set up here and the
/// last sync is ten minutes old. Read without running git, as the
/// scheduler asks every minute.
pub fn due(places: &Places, now_ms: u128) -> bool {
    timed(places)
        && last(places)
            .and_then(|last| last["at_ms"].as_u64())
            .is_some_and(|at| {
                now_ms.saturating_sub(u128::from(at)) >= u128::from(EVERY_SECONDS) * 1000
            })
}

/// Whether the scheduler keeps the backup: `~/agent-home` is a repository
/// and a sync ran, as setup runs the first after restoring. A clone put
/// there by hand is not set up: a sync from it would push this machine's
/// home over the one it holds, before setup restored it.
fn timed(places: &Places) -> bool {
    places.repo.join(".git").exists() && last(places).is_some()
}

/// What the last sync did, with whether backup is set up.
pub fn status(places: &Places) -> Value {
    let last = last(places);
    json!({
        "set_up": done(places),
        "repo": places.repo,
        "remote": origin(&places.repo, None),
        "last": last,
    })
}

/// `sonny setup backup [URL]`: clone or adopt `~/agent-home`, restore the
/// home it holds when this machine has never synced, set up the scheduler and
/// run the first sync.
pub fn set_up(places: &Places, url: Option<&str>) -> Result<(), String> {
    let url = match url {
        Some(url) => url.to_owned(),
        None => {
            if !crate::setup::interactive() {
                return Err("usage: setup backup URL".into());
            }
            println!(
                "Back up memory, skills, profiles and config.toml to a private git repository.\nKeys in ~/.agent/env, logins and chat history stay on this machine.\n"
            );
            crate::setup::line(
                "Repository URL (an empty private repository, or one this set up before): ",
            )?
        }
    };
    if url.is_empty() {
        return Err("setup cancelled".into());
    }
    let restored = adopt(places, &url, None)?;
    if restored {
        println!(
            "Restored {} and {} from {url}.",
            crate::tilde(&places.agents.to_string_lossy()),
            crate::tilde(&places.config.to_string_lossy())
        );
    }
    // The first backup before the scheduler, so a machine whose scheduler
    // cannot run is still backed up once.
    let synced = sync(places, None)?;
    if let Err(error) = crate::scheduler::ensure() {
        println!("Backed up once to {url}, but not again on a timer.");
        return Err(error);
    }
    println!(
        "Backing up to {url} every {} minutes{}.",
        EVERY_SECONDS / 60,
        match synced["config_skipped"].as_str() {
            Some(why) => format!(", without config.toml: {why}"),
            None => String::new(),
        }
    );
    Ok(())
}

/// Make `places.repo` a clone of `url`, or check the one there is; then
/// restore what it holds if this machine never synced. Whether it restored.
fn adopt(places: &Places, url: &str, env: Option<&[(OsString, OsString)]>) -> Result<bool, String> {
    if places.repo.exists() {
        match origin(&places.repo, env) {
            Some(had) if had == url => {}
            Some(had) => {
                return Err(format!(
                    "backup_other_repo: {} already backs up to {had}",
                    places.repo.display()
                ));
            }
            None if places.repo.join(".git").exists() => {
                git(&places.repo, &["remote", "add", "origin", url], env)?;
            }
            None => {
                return Err(format!(
                    "backup_not_a_repo: {} is there and is not a git repository",
                    places.repo.display()
                ));
            }
        }
    } else {
        let parent = places.repo.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let name = places
            .repo
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        git(parent, &["clone", "--quiet", url, &name], env)?;
    }
    // Memory is private to the person, and so is its copy.
    private(&places.repo).map_err(|e| e.to_string())?;
    if ever_synced(places) {
        return Ok(false);
    }
    // Only what the remote holds is restored: a first sync that failed
    // part way left this machine's own copy in the clone, which is no
    // backup, and an empty remote holds nothing.
    if !from_remote(&places.repo, env)? {
        return Ok(false);
    }
    let held = places.repo.join("agents");
    let config = places.repo.join("config.toml");
    if held.is_dir() {
        mirror(&held, &places.agents, false, &Default::default())?;
    }
    if config.is_file() {
        if let Some(dir) = places.config.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        if places.config.is_file() && !same(&config, &places.config) {
            let kept = places.config.with_extension("toml.before-restore");
            std::fs::rename(&places.config, &kept).map_err(|e| e.to_string())?;
            println!(
                "Kept this machine's config as {}.",
                crate::tilde(&kept.to_string_lossy())
            );
        }
        std::fs::copy(&config, &places.config).map_err(|e| e.to_string())?;
        private(&places.config).map_err(|e| e.to_string())?;
    }
    Ok(held.is_dir() || config.is_file())
}

/// Make the clone at `repo` hold the remote's last commit and nothing
/// else, as it is before a restore. Whether the remote holds a commit.
fn from_remote(repo: &Path, env: Option<&[(OsString, OsString)]>) -> Result<bool, String> {
    git(repo, &["fetch", "--quiet", "origin"], env)?;
    // A clone has `origin/HEAD`; a repository given a remote finds it now.
    // An empty remote has neither.
    let _ = git(repo, &["remote", "set-head", "origin", "--auto"], env);
    let Ok(head) = git(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "refs/remotes/origin/HEAD",
        ],
        env,
    ) else {
        return Ok(false);
    };
    git(repo, &["reset", "--quiet", "--hard", head.trim()], env)?;
    git(repo, &["clean", "--quiet", "-ffdx"], env)?;
    Ok(true)
}

/// Whether a sync from this machine ever went through: a machine that only
/// tried, before setup or while offline, still restores first, so its
/// first real sync does not push its fresh home over the one it backs up.
fn ever_synced(places: &Places) -> bool {
    last(places).is_some_and(|last| last["ok_ms"].is_u64())
}

fn last(places: &Places) -> Option<Value> {
    std::fs::read(&places.state)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
}

/// Copy the home into the repository, commit and push. Nothing to commit
/// and nothing unpushed is a sync too.
pub fn sync(places: &Places, env: Option<&[(OsString, OsString)]>) -> Result<Value, String> {
    let result = sync_once(places, env);
    let mut record = match &result {
        Ok(done) => done.clone(),
        Err(error) => error_json(error),
    };
    let now = now_ms();
    record["at_ms"] = json!(now);
    record["ok_ms"] = match &result {
        Ok(_) => json!(now),
        // A failed sync keeps when the last one went through.
        Err(_) => last(places).map_or(Value::Null, |last| last["ok_ms"].clone()),
    };
    if let Some(dir) = places.state.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&places.state, format!("{record}\n"));
    result
}

fn sync_once(places: &Places, env: Option<&[(OsString, OsString)]>) -> Result<Value, String> {
    let repo = &places.repo;
    if origin(repo, env).is_none() {
        return Err("backup_unset: run `sonny setup backup URL` first".into());
    }
    if places.agents.is_dir() {
        let left_out = ignored(repo, "agents", &places.agents, env)?;
        mirror(&places.agents, &repo.join("agents"), true, &left_out)?;
    }
    let mut skipped = None;
    if let Ok(text) = std::fs::read_to_string(&places.config) {
        match secret_in(&text) {
            Some(key) => {
                skipped = Some(format!(
                    "{key} holds a literal secret; move it to ~/.agent/env"
                ));
            }
            None if !same(&places.config, &repo.join("config.toml")) => {
                std::fs::write(repo.join("config.toml"), text).map_err(|e| e.to_string())?;
            }
            None => {}
        }
    }
    git(repo, &["add", "--all"], env)?;
    let changed = !git(repo, &["status", "--porcelain"], env)?
        .trim()
        .is_empty();
    if changed {
        let mut args = vec![];
        // A new machine whose git has no identity yet still backs up.
        for (key, stand_in) in [
            ("user.name", "user.name=Sonny"),
            ("user.email", "user.email=sonny@localhost"),
        ] {
            if git(repo, &["config", key], env).is_err() {
                args.extend(["-c", stand_in]);
            }
        }
        args.extend(["commit", "--quiet", "--message", "Back up home"]);
        git(repo, &args, env)?;
    }
    if changed || unpushed(repo, env) {
        git(
            repo,
            &["push", "--quiet", "--set-upstream", "origin", "HEAD"],
            env,
        )?;
    }
    let commit = git(repo, &["rev-parse", "--short", "HEAD"], env).ok();
    Ok(json!({
        "committed": changed,
        "commit": commit.map(|c| c.trim().to_owned()),
        "config_skipped": skipped,
    }))
}

/// Whether the branch has commits its upstream lacks, or no upstream.
fn unpushed(repo: &Path, env: Option<&[(OsString, OsString)]>) -> bool {
    match git(repo, &["rev-list", "--count", "@{upstream}..HEAD"], env) {
        Ok(count) => count.trim() != "0",
        // No upstream yet, or a branch with no commit: push to set it.
        Err(_) => git(repo, &["rev-parse", "--verify", "--quiet", "HEAD"], env).is_ok(),
    }
}

/// The repository's `origin` URL, if it is one with a remote.
fn origin(repo: &Path, env: Option<&[(OsString, OsString)]>) -> Option<String> {
    if !repo.join(".git").exists() {
        return None;
    }
    git(repo, &["remote", "get-url", "origin"], env)
        .ok()
        .map(|url| url.trim().to_owned())
        .filter(|url| !url.is_empty())
}

/// `git -C dir ARGS`, its output, or its error as `backup_git_failed`.
fn git(dir: &Path, args: &[&str], env: Option<&[(OsString, OsString)]>) -> Result<String, String> {
    let (status, out, err) = run_git(dir, args, env, None)?;
    match status == Some(0) {
        true => Ok(out),
        false => Err(format!(
            "backup_git_failed: git {}: {}",
            args.iter().find(|a| !a.starts_with('-')).unwrap_or(&""),
            err.trim()
        )),
    }
}

/// `git -C dir ARGS` with `input` on its stdin: its exit code, stdout and
/// stderr.
fn run_git(
    dir: &Path,
    args: &[&str],
    env: Option<&[(OsString, OsString)]>,
    input: Option<&str>,
) -> Result<(Option<i32>, String, String), String> {
    use std::io::Write;
    let mut command = std::process::Command::new("git");
    if let Some(env) = env {
        command.env_clear().envs(env.iter().map(|(k, v)| (k, v)));
    }
    for key in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"] {
        command.env_remove(key);
    }
    let mut child = command
        .arg("-C")
        .arg(dir)
        // The person's own git settings must not sign, run hooks, change
        // line endings or leave files out here, as for memory's history:
        // the backup holds every file byte for byte. Their credentials
        // still push.
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.autocrlf=false",
            "-c",
            "core.excludesFile=/dev/null",
            "-c",
            "core.attributesFile=/dev/null",
        ])
        .args(args)
        .stdin(match input {
            Some(_) => std::process::Stdio::piped(),
            None => std::process::Stdio::null(),
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("backup_git_failed: git: {e}"))?;
    // Written beside the reads: git answers as it reads, and an answer that
    // fills its pipe stops it reading.
    let writer = match (input, child.stdin.take()) {
        (Some(text), Some(mut stdin)) => {
            let text = text.to_owned();
            Some(std::thread::spawn(move || stdin.write_all(text.as_bytes())))
        }
        _ => None,
    };
    let out = child
        .wait_with_output()
        .map_err(|e| format!("backup_git_failed: git: {e}"))?;
    if let Some(writer) = writer {
        match writer.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(format!("backup_git_failed: git: {e}")),
            Err(_) => return Err("backup_git_failed: git: its input was not written".into()),
        }
    }
    Ok((
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

/// What under `from` the ignore files leave out, as paths relative to
/// `from`, so the copy at `at` (a folder of the repository) never holds it:
/// walked a level at a time, asking git once a level, and never into a
/// folder left out. The rules are the ones coming in: each folder's
/// `.gitignore` is copied from `from` before its level is asked, and what
/// the copy has where `from` now has a folder goes, as the copy would.
fn ignored(
    repo: &Path,
    at: &str,
    from: &Path,
    env: Option<&[(OsString, OsString)]>,
) -> Result<std::collections::HashSet<PathBuf>, String> {
    let mut left_out = std::collections::HashSet::new();
    let mut level = vec![PathBuf::new()];
    let copy = repo.join(at);
    let fail = |e: std::io::Error| format!("backup_copy_failed: {e}");
    while !level.is_empty() {
        let mut asked = Vec::new();
        for dir in &level {
            let (here, there) = (from.join(dir), copy.join(dir));
            if !std::fs::symlink_metadata(&there).is_ok_and(|m| m.is_dir()) {
                let _ = std::fs::remove_file(&there);
            }
            // A copy that is not a plain file (a link, say) goes first, so
            // the rules never land wherever it points.
            let (rules, kept) = (here.join(".gitignore"), there.join(".gitignore"));
            let plain = std::fs::symlink_metadata(&rules).is_ok_and(|m| m.is_file());
            match std::fs::symlink_metadata(&kept) {
                Ok(m) if m.is_dir() => std::fs::remove_dir_all(&kept).map_err(fail)?,
                Ok(m) if !(plain && m.is_file()) => std::fs::remove_file(&kept).map_err(fail)?,
                _ => {}
            }
            if plain {
                std::fs::create_dir_all(&there).map_err(fail)?;
                std::fs::copy(&rules, &kept).map_err(fail)?;
            }
        }
        for dir in &level {
            let Ok(entries) = std::fs::read_dir(from.join(dir)) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                if SKIP.iter().any(|skip| name == *skip) {
                    continue;
                }
                let folder = entry.file_type().is_ok_and(|kind| kind.is_dir());
                asked.push((dir.join(&name), folder));
            }
        }
        if asked.is_empty() {
            break;
        }
        // A folder is asked with its slash, so a pattern only for folders
        // (`monitor/`) matches it. Git judges a path by what the copy has
        // there too, so what changed kind goes first, as the copy would
        // replace it: what is not a folder where the home has one, and a
        // folder where the home has none (else `cache/` would leave out the
        // home's file `cache`). Paths end in NUL, as git then reads and
        // writes them unquoted.
        for (path, folder) in &asked {
            let there = copy.join(path);
            match std::fs::symlink_metadata(&there) {
                Ok(m) if *folder && !m.is_dir() => std::fs::remove_file(&there).map_err(fail)?,
                Ok(m) if !*folder && m.is_dir() => std::fs::remove_dir_all(&there).map_err(fail)?,
                _ => {}
            }
        }
        let input: String = asked
            .iter()
            .map(|(path, folder)| {
                format!(
                    "{at}/{}{}\0",
                    path.to_string_lossy(),
                    if *folder { "/" } else { "" }
                )
            })
            .collect();
        // Tracked or not: a file the ignore files name now is left out
        // from now on.
        let (status, out, err) = run_git(
            repo,
            &["check-ignore", "--no-index", "--stdin", "-z"],
            env,
            Some(&input),
        )?;
        // 1 is none of them.
        if !matches!(status, Some(0 | 1)) {
            return Err(format!(
                "backup_git_failed: git check-ignore: {}",
                err.trim()
            ));
        }
        let named: std::collections::HashSet<&str> = (out.split('\0'))
            .filter(|path| !path.is_empty())
            .map(|path| path.trim_end_matches('/'))
            .collect();
        level = Vec::new();
        for (path, folder) in asked {
            let full = format!("{at}/{}", path.to_string_lossy());
            if named.contains(full.as_str()) {
                left_out.insert(path);
            } else if folder {
                level.push(path);
            }
        }
    }
    Ok(left_out)
}

/// Make `to` hold what `from` holds, leaving out `SKIP` and what `left_out`
/// names (paths relative to `from`); with `delete`, also remove what `from`
/// no longer has or leaves out. Links are copied as links; what is neither
/// a file, a folder nor a link (a socket, a pipe) is no backup's.
fn mirror(
    from: &Path,
    to: &Path,
    delete: bool,
    left_out: &std::collections::HashSet<PathBuf>,
) -> Result<(), String> {
    mirror_at(from, to, delete, Path::new(""), left_out)
}

fn mirror_at(
    from: &Path,
    to: &Path,
    delete: bool,
    at: &Path,
    left_out: &std::collections::HashSet<PathBuf>,
) -> Result<(), String> {
    let fail =
        |path: &Path, e: std::io::Error| format!("backup_copy_failed: {}: {e}", path.display());
    std::fs::create_dir_all(to).map_err(|e| fail(to, e))?;
    private(to).map_err(|e| fail(to, e))?;
    let mut kept = std::collections::HashSet::new();
    for entry in std::fs::read_dir(from).map_err(|e| fail(from, e))? {
        let entry = entry.map_err(|e| fail(from, e))?;
        let name = entry.file_name();
        if SKIP.iter().any(|skip| name == *skip) || left_out.contains(&at.join(&name)) {
            continue;
        }
        let (source, target) = (entry.path(), to.join(&name));
        let kind = entry.file_type().map_err(|e| fail(&source, e))?;
        if kind.is_symlink() {
            let link = std::fs::read_link(&source).map_err(|e| fail(&source, e))?;
            if std::fs::read_link(&target).ok().as_ref() != Some(&link) {
                remove(&target).map_err(|e| fail(&target, e))?;
                std::os::unix::fs::symlink(&link, &target).map_err(|e| fail(&target, e))?;
            }
        } else if kind.is_dir() {
            if std::fs::symlink_metadata(&target).is_ok_and(|m| !m.is_dir()) {
                remove(&target).map_err(|e| fail(&target, e))?;
            }
            mirror_at(&source, &target, delete, &at.join(&name), left_out)?;
        } else if kind.is_file() {
            // A link or folder in its place is replaced even when what it
            // leads to reads the same.
            let file = std::fs::symlink_metadata(&target).is_ok_and(|m| m.is_file());
            if !file {
                remove(&target).map_err(|e| fail(&target, e))?;
            }
            if !file || !same(&source, &target) {
                std::fs::copy(&source, &target).map_err(|e| fail(&target, e))?;
            }
            own_bits(&source, &target).map_err(|e| fail(&target, e))?;
        } else {
            continue;
        }
        kept.insert(name);
    }
    if delete {
        for entry in std::fs::read_dir(to).map_err(|e| fail(to, e))? {
            let entry = entry.map_err(|e| fail(to, e))?;
            let name = entry.file_name();
            if !kept.contains(&name) && !SKIP.iter().any(|skip| name == *skip) {
                remove(&entry.path()).map_err(|e| fail(&entry.path(), e))?;
            }
        }
    }
    Ok(())
}

/// Only the person may read it, as memory keeps its files: the owner's own
/// bits stay, so a skill's script still runs.
fn private(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)?.permissions().mode();
    if mode & 0o077 != 0 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o700))?;
    }
    Ok(())
}

/// `target` with `source`'s owner bits and no others, whether or not its
/// bytes changed: a script made executable is executable in the copy.
fn own_bits(source: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(source)?.permissions().mode() & 0o700;
    if std::fs::metadata(target)?.permissions().mode() & 0o777 != mode {
        std::fs::set_permissions(target, std::fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

fn remove(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Two files with the same bytes.
fn same(a: &Path, b: &Path) -> bool {
    match (std::fs::read(a), std::fs::read(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// The first key of `config` that holds a secret itself rather than
/// naming the variable it is in: any MCP server's `http_headers`, a
/// string under a key named like a token, key, secret or password, or
/// such a flag's value in an argument list (`--api-key K`,
/// `--token=K`, `Bearer K`). Config that does not parse is held back
/// too.
fn secret_in(config: &str) -> Option<String> {
    let Ok(value) = toml::from_str::<toml::Table>(config) else {
        return Some("config.toml (it does not parse)".into());
    };
    fn secretish(key: &str) -> bool {
        let key = key.to_ascii_lowercase().replace('-', "_");
        !["_env_var", "_env", "_file", "_path"]
            .iter()
            .any(|end| key.ends_with(end))
            && [
                "token",
                "api_key",
                "apikey",
                "secret",
                "password",
                "passwd",
                "private",
                "credential",
            ]
            .iter()
            .any(|word| key.contains(word))
    }
    fn walk(table: &toml::Table, path: &str) -> Option<String> {
        for (key, value) in table {
            let here = match path {
                "" => key.clone(),
                _ => format!("{path}.{key}"),
            };
            match value {
                toml::Value::Table(inner) if key == "http_headers" && !inner.is_empty() => {
                    return Some(here);
                }
                toml::Value::Table(inner) => {
                    if let Some(found) = walk(inner, &here) {
                        return Some(found);
                    }
                }
                toml::Value::String(text) if secretish(key) && !text.is_empty() => {
                    return Some(here);
                }
                toml::Value::Array(items) => {
                    if let Some(found) = in_list(items, &here) {
                        return Some(found);
                    }
                }
                _ => {}
            }
        }
        None
    }
    fn in_list(items: &[toml::Value], path: &str) -> Option<String> {
        let mut flag = false;
        for (at, item) in items.iter().enumerate() {
            let here = format!("{path}[{at}]");
            match item {
                toml::Value::String(text) => {
                    let (name, given) = match text.split_once('=') {
                        Some((name, given)) if name.starts_with('-') => (name, Some(given)),
                        _ => (text.as_str(), None),
                    };
                    let named = name.starts_with('-') && secretish(name.trim_start_matches('-'));
                    let bearer = text.to_ascii_lowercase().starts_with("bearer ");
                    match given {
                        Some(given) if named && !given.is_empty() => return Some(here),
                        _ if bearer || (flag && !text.starts_with('-')) => return Some(here),
                        _ => flag = named && given.is_none(),
                    }
                }
                toml::Value::Table(inner) => {
                    flag = false;
                    if let Some(found) = walk(inner, &here) {
                        return Some(found);
                    }
                }
                toml::Value::Array(inner) => {
                    flag = false;
                    if let Some(found) = in_list(inner, &here) {
                        return Some(found);
                    }
                }
                _ => flag = false,
            }
        }
        None
    }
    walk(&value, "")
}

/// At Sonny's start: the job of its own an earlier Sonny kept let go of,
/// and the scheduler running while the backup is set up.
pub fn at_start() {
    let Ok(places) = Places::home() else {
        return;
    };
    std::thread::spawn(move || {
        if timed(&places)
            && let Err(error) = crate::scheduler::ring()
        {
            eprintln!("{}", error_json(&error));
        }
    });
}

pub(crate) fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sonny-home-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn places(root: &Path) -> Places {
        Places {
            agents: root.join("home/.agents"),
            config: root.join("home/.agent/config.toml"),
            repo: root.join("home/agent-home"),
            state: root.join("home/.agent/sonny/home.json"),
        }
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn sh(dir: &Path, args: &[&str]) -> String {
        git(dir, args, None).unwrap()
    }

    #[test]
    fn a_mirror_leaves_out_repositories_and_memory_s_own_files_and_drops_what_went() {
        let root = scratch("mirror");
        let (from, to) = (root.join("from"), root.join("to"));
        write(&from.join("memory/a.md"), "a");
        write(&from.join("memory/.git/HEAD"), "ref");
        write(&from.join("memory/.reads"), "{}");
        write(&from.join("memory/.said"), "1");
        write(&from.join("skills/plan/SKILL.md"), "plan");
        std::os::unix::fs::symlink("plan", from.join("skills/link")).unwrap();
        write(&to.join("memory/gone.md"), "old");
        mirror(&from, &to, true, &Default::default()).unwrap();
        assert_eq!(
            std::fs::read_to_string(to.join("memory/a.md")).unwrap(),
            "a"
        );
        assert!(to.join("memory/.said").exists());
        assert!(!to.join("memory/.git").exists());
        assert!(!to.join("memory/.reads").exists());
        assert!(!to.join("memory/gone.md").exists());
        assert_eq!(
            std::fs::read_link(to.join("skills/link")).unwrap(),
            Path::new("plan")
        );
        // A script made executable, its bytes unchanged, is executable in the copy.
        {
            use std::os::unix::fs::PermissionsExt;
            let script = from.join("skills/plan/SKILL.md");
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
            mirror(&from, &to, true, &Default::default()).unwrap();
            let mode = std::fs::metadata(to.join("skills/plan/SKILL.md"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        // A link that became a file with the same bytes is a file in the copy.
        std::fs::remove_file(from.join("skills/link")).unwrap();
        write(&from.join("skills/link"), "plan");
        mirror(&from, &to, true, &Default::default()).unwrap();
        assert!(
            std::fs::symlink_metadata(to.join("skills/link"))
                .unwrap()
                .is_file()
        );
        // A restore adds and overwrites but removes nothing.
        write(&to.join("extra.md"), "x");
        mirror(&from, &to, false, &Default::default()).unwrap();
        assert!(to.join("extra.md").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn config_that_holds_a_secret_itself_is_held_back() {
        assert_eq!(
            secret_in("provider = \"chatgpt\"\n[sonny]\nlead_effort = \"medium\"\n"),
            None
        );
        assert_eq!(
            secret_in(
                "[sonny.mcp_servers.linear]\nurl = \"u\"\nbearer_token_env_var = \"LINEAR_TOKEN\"\n"
            ),
            None
        );
        assert_eq!(
            secret_in("[sonny.mcp_servers.x]\nurl = \"u\"\nhttp_headers = { Authorization = \"Bearer abc\" }\n")
                .as_deref(),
            Some("sonny.mcp_servers.x.http_headers")
        );
        assert_eq!(
            secret_in("[sonny.mcp_servers.y]\ncommand = \"y\"\nenv = { Y_API_KEY = \"k\" }\n")
                .as_deref(),
            Some("sonny.mcp_servers.y.env.Y_API_KEY")
        );
        assert_eq!(
            secret_in(
                "[sonny.mcp_servers.z]\ncommand = \"z\"\nenv = { SSH_PRIVATE_KEY = \"k\" }\n"
            )
            .as_deref(),
            Some("sonny.mcp_servers.z.env.SSH_PRIVATE_KEY")
        );
        let args = |list: &str| {
            secret_in(&format!(
                "[sonny.mcp_servers.a]\ncommand = \"a\"\nargs = {list}\n"
            ))
        };
        assert_eq!(
            args(r#"["--api-key", "k"]"#).as_deref(),
            Some("sonny.mcp_servers.a.args[1]")
        );
        assert_eq!(
            args(r#"["serve", "--token=k"]"#).as_deref(),
            Some("sonny.mcp_servers.a.args[1]")
        );
        assert_eq!(
            args(r#"["--header", "Bearer k"]"#).as_deref(),
            Some("sonny.mcp_servers.a.args[1]")
        );
        assert_eq!(args(r#"["--token-file", "/run/t", "--port", "3"]"#), None);
        assert_eq!(args(r#"["--api-key", "--verbose"]"#), None);
        assert_eq!(args(r#"["-y", "@scope/server", "--read-only"]"#), None);
        assert!(secret_in("not = = toml").is_some());
    }

    #[test]
    fn a_sync_pushes_the_home_and_a_new_machine_restores_it() {
        let root = scratch("sync");
        let remote = root.join("remote.git");
        sh(&root, &["init", "--quiet", "--bare", "remote.git"]);
        let url = remote.to_string_lossy().into_owned();
        let first = places(&root.join("one"));
        write(&first.agents.join("memory/fact.md"), "fact");
        write(&first.agents.join("memory/.git/HEAD"), "ref");
        write(&first.config, "provider = \"chatgpt\"\n");
        assert!(
            !adopt(&first, &url, None).unwrap(),
            "an empty repository restores nothing"
        );
        sh(&first.repo, &["config", "user.email", "t@example.com"]);
        sh(&first.repo, &["config", "user.name", "T"]);
        let done = sync(&first, None).unwrap();
        assert_eq!(done["committed"], true);
        assert_eq!(sync(&first, None).unwrap()["committed"], false);
        assert!(first.state.exists());
        let held = sh(&remote, &["ls-tree", "-r", "--name-only", "HEAD"]);
        assert_eq!(
            held.lines().collect::<Vec<_>>(),
            ["agents/memory/fact.md", "config.toml"]
        );

        // Another machine, already set up with its own config.
        let second = places(&root.join("two"));
        write(&second.config, "provider = \"anthropic\"\n");
        assert!(adopt(&second, &url, None).unwrap());
        assert_eq!(
            std::fs::read_to_string(second.agents.join("memory/fact.md")).unwrap(),
            "fact"
        );
        assert_eq!(
            std::fs::read_to_string(&second.config).unwrap(),
            "provider = \"chatgpt\"\n"
        );
        assert!(second.config.with_extension("toml.before-restore").exists());
        // A second repository is refused rather than adopted.
        let other = format!("{url}-other");
        assert!(
            adopt(&second, &other, None)
                .unwrap_err()
                .starts_with("backup_other_repo: ")
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_person_s_global_ignores_leave_nothing_out() {
        let root = scratch("ignores");
        sh(&root, &["init", "--quiet", "--bare", "remote.git"]);
        write(&root.join("ignore"), "config.toml\n*.md\n");
        write(
            &root.join("gitconfig"),
            &format!(
                "[core]\nexcludesFile = {}\n[user]\nname = T\nemail = t@example.com\n",
                root.join("ignore").display()
            ),
        );
        let mut env: Vec<(OsString, OsString)> = std::env::vars_os()
            .filter(|(key, _)| key != "GIT_CONFIG_GLOBAL")
            .collect();
        env.push(("GIT_CONFIG_GLOBAL".into(), root.join("gitconfig").into()));
        let at = places(&root.join("one"));
        write(&at.agents.join("memory/fact.md"), "fact");
        write(&at.config, "provider = \"chatgpt\"\n");
        adopt(&at, &root.join("remote.git").to_string_lossy(), Some(&env)).unwrap();
        sync(&at, Some(&env)).unwrap();
        let held = sh(
            &root.join("remote.git"),
            &["ls-tree", "-r", "--name-only", "HEAD"],
        );
        assert_eq!(
            held.lines().collect::<Vec<_>>(),
            ["agents/memory/fact.md", "config.toml"]
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_config_with_a_secret_is_left_out_while_memory_still_goes() {
        let root = scratch("secret");
        sh(&root, &["init", "--quiet", "--bare", "remote.git"]);
        let at = places(&root.join("one"));
        write(&at.agents.join("memory/fact.md"), "fact");
        write(
            &at.config,
            "[sonny.mcp_servers.x]\nurl = \"u\"\nhttp_headers = { A = \"b\" }\n",
        );
        adopt(&at, &root.join("remote.git").to_string_lossy(), None).unwrap();
        sh(&at.repo, &["config", "user.email", "t@example.com"]);
        sh(&at.repo, &["config", "user.name", "T"]);
        let done = sync(&at, None).unwrap();
        assert!(
            done["config_skipped"]
                .as_str()
                .unwrap()
                .starts_with("sonny.mcp_servers.x.http_headers")
        );
        assert!(!at.repo.join("config.toml").exists());
        assert!(at.repo.join("agents/memory/fact.md").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_machine_whose_syncs_never_went_through_restores_first_and_keeps_memory_private() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch("tried");
        sh(&root, &["init", "--quiet", "--bare", "remote.git"]);
        let url = root.join("remote.git").to_string_lossy().into_owned();
        let first = places(&root.join("one"));
        write(&first.agents.join("memory/fact.md"), "fact");
        adopt(&first, &url, None).unwrap();
        sh(&first.repo, &["config", "user.email", "t@example.com"]);
        sh(&first.repo, &["config", "user.name", "T"]);
        sync(&first, None).unwrap();

        // A new machine tried a sync before setup, and Sonny wrote its skills.
        let second = places(&root.join("two"));
        assert!(sync(&second, None).is_err());
        write(&second.agents.join("skills/plan/SKILL.md"), "plan");
        assert!(adopt(&second, &url, None).unwrap());
        let fact = second.agents.join("memory/fact.md");
        assert_eq!(std::fs::read_to_string(&fact).unwrap(), "fact");
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&fact) & 0o077, 0, "{:o}", mode(&fact));
        assert_eq!(mode(&second.repo) & 0o077, 0);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_backup_of_config_alone_restores_it() {
        let root = scratch("config");
        sh(&root, &["init", "--quiet", "--bare", "remote.git"]);
        let url = root.join("remote.git").to_string_lossy().into_owned();
        let first = places(&root.join("one"));
        write(&first.config, "provider = \"chatgpt\"\n");
        adopt(&first, &url, None).unwrap();
        sh(&first.repo, &["config", "user.email", "t@example.com"]);
        sh(&first.repo, &["config", "user.name", "T"]);
        sync(&first, None).unwrap();
        let second = places(&root.join("two"));
        assert!(adopt(&second, &url, None).unwrap());
        assert_eq!(
            std::fs::read_to_string(&second.config).unwrap(),
            "provider = \"chatgpt\"\n"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_socket_in_the_home_is_left_out_of_the_copy() {
        let root = scratch("socket");
        let (from, to) = (root.join("from"), root.join("to"));
        write(&from.join("memory/a.md"), "a");
        let _listener =
            std::os::unix::net::UnixListener::bind(from.join("memory/daemon.sock")).unwrap();
        mirror(&from, &to, true, &Default::default()).unwrap();
        assert!(to.join("memory/a.md").exists());
        assert!(!to.join("memory/daemon.sock").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn what_the_repository_ignores_is_never_copied_and_a_copy_already_there_goes() {
        let root = scratch("ignored");
        sh(&root, &["init", "--quiet", "--bare", "remote.git"]);
        let at = places(&root.join("one"));
        write(&at.agents.join("memory/fact.md"), "fact");
        write(&at.agents.join("monitor/inbox/mail.json"), "mail");
        write(&at.agents.join("skills/plan/cache.tmp"), "tmp");
        adopt(&at, &root.join("remote.git").to_string_lossy(), None).unwrap();
        sh(&at.repo, &["config", "user.email", "t@example.com"]);
        sh(&at.repo, &["config", "user.name", "T"]);
        sync(&at, None).unwrap();
        assert!(at.repo.join("agents/monitor/inbox/mail.json").exists());
        // The agent that set it up leaves monitor content out.
        write(&at.repo.join(".gitignore"), "agents/monitor/\n*.tmp\n");
        sync(&at, None).unwrap();
        assert!(at.repo.join("agents/memory/fact.md").exists());
        assert!(!at.repo.join("agents/monitor").exists());
        assert!(!at.repo.join("agents/skills/plan/cache.tmp").exists());
        let held = sh(
            &root.join("remote.git"),
            &["ls-tree", "-r", "--name-only", "HEAD"],
        );
        assert_eq!(
            held.lines().collect::<Vec<_>>(),
            [".gitignore", "agents/memory/fact.md"]
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_home_s_own_new_ignore_rules_hold_from_the_sync_that_brings_them() {
        let root = scratch("rules");
        sh(&root, &["init", "--quiet", "--bare", "remote.git"]);
        let at = places(&root.join("one"));
        write(&at.agents.join("memory/fact.md"), "fact");
        write(
            &at.agents.join("memory/café.md"),
            "kept until a rule names it",
        );
        std::fs::create_dir_all(at.agents.join("skills")).unwrap();
        std::os::unix::fs::symlink("/nowhere", at.agents.join("skills/plan")).unwrap();
        adopt(&at, &root.join("remote.git").to_string_lossy(), None).unwrap();
        sh(&at.repo, &["config", "user.email", "t@example.com"]);
        sh(&at.repo, &["config", "user.name", "T"]);
        sync(&at, None).unwrap();
        assert!(at.repo.join("agents/memory/café.md").exists());
        // A rule in the home, for a name git would quote, and a link that
        // became a folder.
        write(&at.agents.join("memory/café.md"), "a secret now");
        write(&at.agents.join("memory/.gitignore"), "café.md\n");
        std::fs::remove_file(at.agents.join("skills/plan")).unwrap();
        write(&at.agents.join("skills/plan/SKILL.md"), "plan");
        sync(&at, None).unwrap();
        let held = |what: &str| {
            sh(
                &root.join("remote.git"),
                &["log", "--all", "-p", "--", what],
            )
        };
        assert!(!held("agents/memory/café.md").contains("a secret now"));
        let tree = sh(
            &root.join("remote.git"),
            &["ls-tree", "-r", "--name-only", "-z", "HEAD"],
        );
        assert_eq!(
            tree.split('\0')
                .filter(|p| !p.is_empty())
                .collect::<Vec<_>>(),
            [
                "agents/memory/.gitignore",
                "agents/memory/fact.md",
                "agents/skills/plan/SKILL.md"
            ]
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_rules_link_that_became_a_file_never_writes_where_it_pointed() {
        let root = scratch("rules-link");
        sh(&root, &["init", "--quiet", "--bare", "remote.git"]);
        let at = places(&root.join("one"));
        let outside = root.join("outside.txt");
        write(&outside, "untouched");
        write(&at.agents.join("memory/fact.md"), "fact");
        std::os::unix::fs::symlink(&outside, at.agents.join("memory/.gitignore")).unwrap();
        adopt(&at, &root.join("remote.git").to_string_lossy(), None).unwrap();
        sh(&at.repo, &["config", "user.email", "t@example.com"]);
        sh(&at.repo, &["config", "user.name", "T"]);
        sync(&at, None).unwrap();
        assert!(
            std::fs::symlink_metadata(at.repo.join("agents/memory/.gitignore"))
                .unwrap()
                .is_symlink()
        );
        // The home's link becomes rules of its own.
        std::fs::remove_file(at.agents.join("memory/.gitignore")).unwrap();
        write(&at.agents.join("memory/.gitignore"), "fact.md\n");
        sync(&at, None).unwrap();
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "untouched");
        assert!(!at.repo.join("agents/memory/fact.md").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_folder_that_became_a_file_is_kept_past_a_rule_for_folders() {
        let root = scratch("folder-file");
        sh(&root, &["init", "--quiet", "--bare", "remote.git"]);
        let at = places(&root.join("one"));
        write(&at.agents.join("cache/old.md"), "old");
        adopt(&at, &root.join("remote.git").to_string_lossy(), None).unwrap();
        sh(&at.repo, &["config", "user.email", "t@example.com"]);
        sh(&at.repo, &["config", "user.name", "T"]);
        sync(&at, None).unwrap();
        assert!(at.repo.join("agents/cache/old.md").exists());
        std::fs::remove_dir_all(at.agents.join("cache")).unwrap();
        write(&at.agents.join("cache"), "a file now");
        write(&at.agents.join(".gitignore"), "cache/\n");
        sync(&at, None).unwrap();
        assert_eq!(
            std::fs::read_to_string(at.repo.join("agents/cache")).unwrap(),
            "a file now"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn many_paths_at_one_level_are_checked_without_a_stall() {
        let root = scratch("many");
        sh(&root, &["init", "--quiet", "--bare", "remote.git"]);
        let at = places(&root.join("one"));
        for n in 0..3000 {
            write(
                &at.agents
                    .join(format!("cache/entry-with-a-long-name-{n:05}.tmp")),
                "",
            );
        }
        write(&at.agents.join("memory/fact.md"), "fact");
        adopt(&at, &root.join("remote.git").to_string_lossy(), None).unwrap();
        write(&at.repo.join(".gitignore"), "*.tmp\n");
        sh(&at.repo, &["config", "user.email", "t@example.com"]);
        sh(&at.repo, &["config", "user.name", "T"]);
        sync(&at, None).unwrap();
        let copied = std::fs::read_dir(at.repo.join("agents/cache")).map_or(0, |d| d.count());
        assert_eq!(copied, 0);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_first_sync_that_failed_part_way_restores_nothing_over_the_home() {
        let root = scratch("partial");
        sh(&root, &["init", "--quiet", "--bare", "remote.git"]);
        let url = root.join("remote.git").to_string_lossy().into_owned();
        let at = places(&root.join("one"));
        write(&at.agents.join("memory/fact.md"), "old");
        assert!(!adopt(&at, &url, None).unwrap());
        // The first sync copied an older home and failed before it pushed.
        mirror(
            &at.agents,
            &at.repo.join("agents"),
            true,
            &Default::default(),
        )
        .unwrap();
        write(&at.agents.join("memory/fact.md"), "new");
        assert!(!adopt(&at, &url, None).unwrap(), "the remote holds nothing");
        assert_eq!(
            std::fs::read_to_string(at.agents.join("memory/fact.md")).unwrap(),
            "new"
        );

        // With a backup there, a half-done copy gives way to what it holds.
        let first = places(&root.join("two"));
        write(&first.agents.join("memory/fact.md"), "backed up");
        adopt(&first, &url, None).unwrap();
        sh(&first.repo, &["config", "user.email", "t@example.com"]);
        sh(&first.repo, &["config", "user.name", "T"]);
        sync(&first, None).unwrap();
        write(&at.repo.join("agents/memory/stray.md"), "stray");
        assert!(adopt(&at, &url, None).unwrap());
        assert_eq!(
            std::fs::read_to_string(at.agents.join("memory/fact.md")).unwrap(),
            "backed up"
        );
        assert!(!at.agents.join("memory/stray.md").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_sync_before_setup_is_backup_unset() {
        let root = scratch("unset");
        let error = sync(&places(&root), None).unwrap_err();
        assert!(error.starts_with("backup_unset: "), "{error}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_sync_is_due_ten_minutes_after_the_last() {
        let root = scratch("due");
        let places = places(&root);
        assert!(!due(&places, 1_000_000), "no repository, no sync");
        std::fs::create_dir_all(places.repo.join(".git")).unwrap();
        assert!(!due(&places, 1_000_000), "a clone setup never synced from");
        write(&places.state, "{\"at_ms\": 1000000}");
        assert!(!due(&places, 1_000_000 + 599_999));
        assert!(due(&places, 1_000_000 + 600_000));
    }
}
