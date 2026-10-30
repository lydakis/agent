//! The person's home kept off the machine: `~/.agents` (memory, skills,
//! profiles) and `~/.agent/config.toml`, copied into a git repository,
//! `~/agent-home`, and pushed every ten minutes by the system's scheduler,
//! so a machine that dies takes at most that much with it. Keys
//! (`~/.agent/env`), logins and the daemon's store stay out.
//!
//! `sonny setup backup URL` sets it up, and restores a home the repository
//! already holds, which is how a new machine starts; `sonny home sync` is
//! what the job runs, and `sonny home status` says how the last one went.
//!
//! Memory is copied without its own `.git`: a commit inside
//! `~/.agents/memory` would make `memory changed` find nothing and skip the
//! nightly cleanup.
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

/// The job's launchd label and systemd unit name.
const LABEL: &str = "me.lydakis.agent.backup";
/// How often the job runs.
pub const EVERY_SECONDS: u32 = 600;
/// Names never copied, at any depth: a repository's own, and what memory
/// keeps out of its history (a lock, read counts, an open cleanup that
/// names a commit the copy does not have).
const SKIP: [&str; 5] = [".git", ".lock", ".reads", ".cleanup", ".DS_Store"];
pub const USAGE: &str = "usage: home sync | home status\n       setup backup [URL]";

/// Where the home, its copy and the job live.
pub struct Places {
    agents: PathBuf,
    config: PathBuf,
    repo: PathBuf,
    state: PathBuf,
    job: PathBuf,
}

impl Places {
    pub fn home() -> Result<Self, String> {
        let home = PathBuf::from(std::env::var_os("HOME").ok_or("set HOME")?);
        let jobs = crate::trigger::Places::home()?;
        let job = match jobs.scheduler {
            crate::trigger::Scheduler::Launchd => jobs.jobs.join(format!("{LABEL}.plist")),
            crate::trigger::Scheduler::Systemd => jobs.jobs.join(format!("{LABEL}.service")),
        };
        Ok(Self {
            agents: home.join(".agents"),
            config: home.join(".agent/config.toml"),
            repo: home.join("agent-home"),
            state: home.join(".agent/sonny/home.json"),
            job,
        })
    }
}

/// `sonny home ARGS`: one JSON value on stdout, or a typed error on stderr.
pub fn cli(args: &[String]) -> i32 {
    let done = Places::home().and_then(|places| match args {
        [verb] if verb == "sync" => sync(&places, None),
        // The scheduler's run: git with the login shell's environment,
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

/// Set up for this setup step: a repository with a remote, and the job.
pub fn done(places: &Places) -> bool {
    places.job.exists() && origin(&places.repo, None).is_some()
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
/// home it holds when this machine has never synced, install the job and
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
    // The first backup before the job, so a machine whose scheduler
    // refuses it is still backed up once.
    let synced = sync(places, None)?;
    if let Err(error) = install_job(places) {
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
    let held = places.repo.join("agents");
    let config = places.repo.join("config.toml");
    if held.is_dir() {
        mirror(&held, &places.agents, false)?;
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
        mirror(&places.agents, &repo.join("agents"), true)?;
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
    let mut command = std::process::Command::new("git");
    if let Some(env) = env {
        command.env_clear().envs(env.iter().map(|(k, v)| (k, v)));
    }
    for key in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"] {
        command.env_remove(key);
    }
    let out = command
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
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("backup_git_failed: git: {e}"))?;
    match out.status.success() {
        true => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
        false => Err(format!(
            "backup_git_failed: git {}: {}",
            args.iter().find(|a| !a.starts_with('-')).unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        )),
    }
}

/// Make `to` hold what `from` holds, leaving out `SKIP`; with `delete`,
/// also remove what `from` no longer has. Links are copied as links.
fn mirror(from: &Path, to: &Path, delete: bool) -> Result<(), String> {
    let fail =
        |path: &Path, e: std::io::Error| format!("backup_copy_failed: {}: {e}", path.display());
    std::fs::create_dir_all(to).map_err(|e| fail(to, e))?;
    private(to).map_err(|e| fail(to, e))?;
    let mut kept = std::collections::HashSet::new();
    for entry in std::fs::read_dir(from).map_err(|e| fail(from, e))? {
        let entry = entry.map_err(|e| fail(from, e))?;
        let name = entry.file_name();
        if SKIP.iter().any(|skip| name == *skip) {
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
            mirror(&source, &target, delete)?;
        } else {
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

/// At Sonny's start: a job set up by an earlier Sonny runs this one, as
/// triggers are moved to it.
pub fn at_start() {
    let (Ok(places), Some(sonny)) = (Places::home(), crate::skills::sonny_path()) else {
        return;
    };
    let Ok(text) = std::fs::read_to_string(&places.job) else {
        return;
    };
    // Only the program decides: the environment is the shell's that set it up.
    let program = match crate::trigger::Scheduler::here() {
        crate::trigger::Scheduler::Launchd => {
            format!(
                "<string>{}</string>",
                crate::trigger::escape(&sonny.to_string_lossy())
            )
        }
        crate::trigger::Scheduler::Systemd => {
            format!(
                "ExecStart={} ",
                crate::trigger::quoted(&sonny.to_string_lossy(), true)
            )
        }
    };
    if !text.contains(&program) {
        std::thread::spawn(move || {
            let _ = install_job(&places);
        });
    }
}

/// Write the job's definition and load it, replacing one loaded before.
fn install_job(places: &Places) -> Result<(), String> {
    let sonny = crate::skills::sonny_path().ok_or("backup_failed: Sonny's own path is unknown")?;
    let scheduler = crate::trigger::Scheduler::here();
    let text = definition(scheduler, &sonny, &crate::trigger::environment(scheduler));
    if let Some(dir) = places.job.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    if scheduler == crate::trigger::Scheduler::Launchd {
        // bootstrap refuses a label already loaded.
        let _ = crate::trigger::schedule(crate::trigger::Job::Unload(LABEL));
    }
    std::fs::write(&places.job, text).map_err(|e| e.to_string())?;
    let loaded = crate::trigger::schedule(crate::trigger::Job::Load(&places.job));
    // A job the scheduler refused is not set up.
    if loaded.is_err() {
        let _ = std::fs::remove_file(&places.job);
    }
    loaded
}

/// The job: `sonny home sync --job` every ten minutes, a LaunchAgent on a
/// Mac, a user service whose timer catches up after sleep elsewhere. It
/// runs with `environment` (HOME, SHELL, PATH as triggers keep them), so
/// the sync finds the login shell whose credentials git pushes with.
fn definition(
    scheduler: crate::trigger::Scheduler,
    sonny: &Path,
    environment: &[(&str, String)],
) -> String {
    let sonny = sonny.to_string_lossy();
    match scheduler {
        crate::trigger::Scheduler::Launchd => format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key>\n  <string>{LABEL}</string>\n  <key>ProgramArguments</key>\n  <array>\n    <string>{}</string>\n    <string>home</string>\n    <string>sync</string>\n    <string>--job</string>\n  </array>\n  <key>StartInterval</key>\n  <integer>{EVERY_SECONDS}</integer>\n{}  <key>ProcessType</key>\n  <string>Background</string>\n</dict>\n</plist>\n",
            crate::trigger::escape(&sonny),
            crate::trigger::environment_section(environment)
        ),
        crate::trigger::Scheduler::Systemd => format!(
            "# Sonny's backup of ~/.agents and config.toml to ~/agent-home.\n[Unit]\nDescription=Sonny home backup\n\n[Service]\nType=oneshot\nExecStart={} home sync --job\n{}\n[X-Timer]\nOnCalendar=*:0/{}\nPersistent=true\n",
            crate::trigger::quoted(&sonny, true),
            crate::trigger::unit_environment(environment),
            EVERY_SECONDS / 60
        ),
    }
}

fn now_ms() -> u128 {
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
            job: root.join("jobs/backup.service"),
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
        mirror(&from, &to, true).unwrap();
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
            mirror(&from, &to, true).unwrap();
            let mode = std::fs::metadata(to.join("skills/plan/SKILL.md"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        // A link that became a file with the same bytes is a file in the copy.
        std::fs::remove_file(from.join("skills/link")).unwrap();
        write(&from.join("skills/link"), "plan");
        mirror(&from, &to, true).unwrap();
        assert!(
            std::fs::symlink_metadata(to.join("skills/link"))
                .unwrap()
                .is_file()
        );
        // A restore adds and overwrites but removes nothing.
        write(&to.join("extra.md"), "x");
        mirror(&from, &to, false).unwrap();
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
    fn a_sync_before_setup_is_backup_unset() {
        let root = scratch("unset");
        let error = sync(&places(&root), None).unwrap_err();
        assert!(error.starts_with("backup_unset: "), "{error}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_job_runs_the_sync_every_ten_minutes() {
        let sonny = Path::new("/opt/homebrew/opt/sonny/bin/sonny");
        let environment = [
            ("HOME", "/Users/me".to_owned()),
            ("SHELL", "/bin/zsh".to_owned()),
        ];
        let plist = definition(crate::trigger::Scheduler::Launchd, sonny, &environment);
        assert!(plist.contains("<string>me.lydakis.agent.backup</string>"));
        assert!(plist.contains("<string>/opt/homebrew/opt/sonny/bin/sonny</string>\n    <string>home</string>\n    <string>sync</string>\n    <string>--job</string>"));
        assert!(plist.contains("<key>StartInterval</key>\n  <integer>600</integer>"));
        assert!(plist.contains("<key>SHELL</key><string>/bin/zsh</string>"));
        let unit = definition(
            crate::trigger::Scheduler::Systemd,
            Path::new("/home/me/.local/bin/sonny"),
            &environment,
        );
        assert!(unit.contains("ExecStart=\"/home/me/.local/bin/sonny\" home sync --job\n"));
        assert!(unit.contains("[X-Timer]\nOnCalendar=*:0/10\nPersistent=true\n"));
        assert!(unit.contains("Environment=\"SHELL=/bin/zsh\"\n"));
    }
}
