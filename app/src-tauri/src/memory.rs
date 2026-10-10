//! Memory: facts agents save for later agents, one Markdown file each. The
//! person's are in `~/.agents/memory`; a project's are in
//! `~/.agents/memory/projects/NAME`, outside its checkout, so its lead and
//! every task's worktree share one copy whatever is committed. Each folder's
//! `MEMORY.md` is an index generated from the facts' front matter and kept
//! within 4 KiB, so reading it at the start of every task stays cheap; a
//! save that would pass that is refused until facts are merged or removed.
//! The daemon knows nothing of memory.
//!
//! `~/.agent/memory`, a script the app writes, shows the indexes and saves,
//! removes, indexes and checks facts; the `memory` skill the app ships says what to save.
//! `schedule` adds a nightly trigger whose agent follows the `memory-cleanup`
//! skill between `cleanup start` and `cleanup finish`, which commit to a local
//! git repository in `~/.agents/memory` and revert a cleanup that removed too
//! many facts; `changed` gates the trigger so a quiet night costs no turn.
use serde_json::{Value, json};
use std::{
    io::Read,
    path::{Path, PathBuf},
};

pub const FLAG: &str = "--memory";
/// A fact file, front matter included, and a folder's index: each at most
/// this. Small on purpose, like Hermes' and Claude Code's memory indexes.
const LIMIT: usize = 4096;
/// Entries a folder may hold, facts or not, before it is refused as not a
/// memory folder.
const MAX_ENTRIES: usize = 1024;
const INDEX: &str = "MEMORY.md";
const TYPES: [&str; 4] = ["user", "feedback", "project", "reference"];
const USAGE: &str = "usage: memory show [SCOPE]\n       memory save NAME --type user|feedback|project|reference --description TEXT --source TEXT [SCOPE] -- TEXT|-\n       memory rm NAME [SCOPE]\n       memory index [SCOPE]\n       memory check [SCOPE]\n       memory changed | cleanup start | cleanup finish\n       memory schedule --model PROVIDER/MODEL [--effort LEVEL] [--cron 'MIN HOUR DAY MONTH WEEKDAY']\n         SCOPE: --user | --project NAME; none: the project of this folder";

/// `APP --memory show|save|rm|index|check|changed|cleanup|schedule`, from
/// `~/.agent/memory`.
pub fn cli(args: &[String]) -> i32 {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let cwd = std::env::current_dir().ok();
    let done = match (home, cwd) {
        (Some(home), _) if args.first().is_some_and(|verb| verb == "schedule") => {
            schedule(&home, &args[1..])
        }
        (Some(home), Some(cwd)) => run(&home.join(".agents/memory"), &cwd, args, &stdin),
        _ => Err("memory_failed: no HOME or working folder".into()),
    };
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

fn stdin() -> Result<String, String> {
    let mut text = String::new();
    std::io::stdin()
        .take(LIMIT as u64 + 1)
        .read_to_string(&mut text)
        .map_err(|e| format!("invalid_text: stdin: {e}"))?;
    Ok(text)
}

/// One error shape, `{"error": CODE, "detail": ...}`, from the
/// `CODE: detail` this module's errors are.
fn error_json(message: &str) -> Value {
    match message.split_once(": ") {
        Some((code, detail))
            if !code.is_empty() && code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') =>
        {
            json!({"error": code, "detail": detail})
        }
        _ => json!({"error": "memory_failed", "detail": message}),
    }
}

fn run(
    root: &Path,
    cwd: &Path,
    args: &[String],
    stdin: &dyn Fn() -> Result<String, String>,
) -> Result<Value, String> {
    let usage = || format!("usage: {USAGE}");
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["changed"] => return changed(root),
        ["cleanup", "start"] => return cleanup_start(root),
        ["cleanup", "finish"] => return cleanup_finish(root),
        _ => {}
    }
    let (verb, rest) = args.split_first().ok_or_else(usage)?;
    let (flags, text) = match rest.iter().position(|a| a == "--") {
        Some(at) if verb == "save" => (&rest[..at], Some(rest[at + 1..].join(" "))),
        _ => (rest, None),
    };
    let mut name = None;
    let mut fields = [None, None, None];
    let mut scope = None;
    let mut it = flags.iter();
    while let Some(arg) = it.next() {
        let slot = match arg.as_str() {
            "--type" => &mut fields[0],
            "--description" => &mut fields[1],
            "--source" => &mut fields[2],
            "--project" => &mut scope,
            "--user" if scope.is_none() => {
                scope = Some(String::new());
                continue;
            }
            flag if flag.starts_with('-') => return Err(usage()),
            _ if name.is_none() && matches!(verb.as_str(), "save" | "rm") => {
                name = Some(arg.clone());
                continue;
            }
            _ => return Err(usage()),
        };
        if slot.is_some() {
            return Err(usage());
        }
        *slot = Some(it.next().ok_or_else(usage)?.clone());
    }
    // The scope is resolved once the command is known to be whole, so a
    // usage error says so even outside a project.
    let project = || -> Result<Option<String>, String> {
        Ok(match &scope {
            Some(user) if user.is_empty() => None,
            Some(project) => {
                if !crate::project::valid_name(project) {
                    return Err(format!(
                        "invalid_project: {project}: a project name is 1-64 of A-Z a-z 0-9 - _ ."
                    ));
                }
                Some(project.clone())
            }
            None => Some(project_of(cwd)?),
        })
    };
    let scope = || -> Result<PathBuf, String> {
        Ok(match project()? {
            None => root.to_path_buf(),
            Some(project) => root.join("projects").join(project),
        })
    };
    match (verb.as_str(), name, fields, text) {
        ("show", None, [None, None, None], None) => {
            // Outside a project, the person's index alone.
            let project = match project() {
                Err(error) if error.starts_with("project_unknown: ") => None,
                project => project?,
            };
            let index = |dir: PathBuf| {
                let text = std::fs::read_to_string(dir.join(INDEX)).unwrap_or_default();
                json!({"dir": dir, "index": text})
            };
            let mut shown = json!({"user": index(root.to_path_buf()), "project": null});
            if let Some(name) = project {
                shown["project"] = index(root.join("projects").join(&name));
                shown["project"]["name"] = json!(name);
            }
            Ok(shown)
        }
        ("save", Some(name), [Some(kind), Some(description), Some(source)], Some(text)) => {
            let text = if text == "-" { stdin()? } else { text };
            // Standard input is read to one byte past the limit, so a fact
            // whose end went unread is refused rather than cut.
            if text.len() > LIMIT {
                return Err(format!(
                    "fact_too_large: the text is more than {LIMIT} bytes; save one fact per file"
                ));
            }
            let fact = Fact {
                name,
                kind,
                description,
                source,
                verified: today(),
            };
            save(&scope()?, &fact, &text)
        }
        ("rm", Some(name), [None, None, None], None) => remove(&scope()?, &name),
        ("index", None, [None, None, None], None) => {
            let dir = scope()?;
            let _lock = lock(&dir)?;
            let all = facts(&dir)?;
            let text = render_index(&dir, &all);
            if text.len() > LIMIT {
                return Err(format!(
                    "memory_full: the index would be {} bytes, above {LIMIT}; merge facts or remove one with memory rm NAME first",
                    text.len()
                ));
            }
            write(&dir.join(INDEX), &text)?;
            Ok(
                json!({"index": dir.join(INDEX), "bytes": text.len(), "facts": all.len(), "limit": LIMIT}),
            )
        }
        ("check", None, [None, None, None], None) => Ok(check(&scope()?)),
        _ => Err(usage()),
    }
}

/// The project a folder belongs to, as the client composing a new agent
/// finds it: the nearest `.agents/project.toml`, looked up from a task's
/// worktree at the same place in the repository's main checkout.
fn project_of(cwd: &Path) -> Result<String, String> {
    match agent_client::policy::project(cwd) {
        Ok(Some(name)) => Ok(name),
        Ok(None) => Err(format!(
            "project_unknown: {}: no .agents/project.toml at or above this folder; pass --project NAME or --user",
            cwd.display()
        )),
        Err(error) => Err(format!("project_invalid: {error}")),
    }
}

/// The local date, for a fact's `verified`.
fn today() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let day = crate::trigger::local(now);
    format!("{:04}-{:02}-{:02}", day.year, day.month, day.day)
}

#[derive(Debug, Clone, PartialEq)]
struct Fact {
    name: String,
    kind: String,
    description: String,
    source: String,
    verified: String,
}

impl Fact {
    fn render(&self, body: &str) -> String {
        format!(
            "---\nname: {}\ndescription: {}\ntype: {}\nsource: {}\nverified: {}\n---\n\n{}\n",
            self.name,
            self.description,
            self.kind,
            self.source,
            self.verified,
            body.trim()
        )
    }

    fn line(&self) -> String {
        format!(
            "- {} ({}, verified {}): {}\n",
            self.name, self.kind, self.verified, self.description
        )
    }

    /// The fact a file holds, or what is wrong with it.
    fn parse(file: &str, text: &str) -> Result<Self, String> {
        let rest = text
            .strip_prefix("---\n")
            .ok_or("no front matter: the file must start with ---")?;
        let (head, _) = rest
            .split_once("\n---\n")
            .ok_or("front matter opened with --- is not closed")?;
        let mut fields = std::collections::HashMap::new();
        for line in head.lines() {
            let (key, value) = line
                .split_once(": ")
                .ok_or_else(|| format!("front matter line {line:?} is not KEY: VALUE"))?;
            fields.insert(key, value);
        }
        let field = |key: &str| {
            fields
                .get(key)
                .map(|v| v.to_string())
                .ok_or_else(|| format!("front matter has no {key}"))
        };
        let fact = Self {
            name: field("name")?,
            kind: field("type")?,
            description: field("description")?,
            source: field("source")?,
            verified: field("verified")?,
        };
        if format!("{}.md", fact.name) != file {
            return Err(format!("its name is {}, not its file's", fact.name));
        }
        fact.valid()?;
        Ok(fact)
    }

    fn valid(&self) -> Result<(), String> {
        valid_name(&self.name)?;
        if !TYPES.contains(&self.kind.as_str()) {
            return Err(format!(
                "invalid_type: {}: one of {}",
                self.kind,
                TYPES.join(", ")
            ));
        }
        for (field, value) in [("description", &self.description), ("source", &self.source)] {
            if value.trim().is_empty() || value.contains('\n') || value.trim() != value {
                return Err(format!(
                    "invalid_{field}: one line, not empty, no leading or trailing space"
                ));
            }
        }
        Ok(())
    }
}

/// A fact's name, which is also its file's. `memory` would be the index's
/// file on a Mac, whose folders ignore case.
fn valid_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 64
        || name.starts_with('-')
        || name == "memory"
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(format!(
            "invalid_name: {name}: a fact's name is 1-64 of a-z 0-9 -, not starting with - and not memory"
        ));
    }
    Ok(())
}

/// The folder's lock, held while a change and the index it implies are
/// written, so two agents saving at once cannot leave an index that misses
/// one of them.
fn lock(dir: &Path) -> Result<std::fs::File, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("memory_failed: {}: {e}", dir.display()))?;
    let path = dir.join(".lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| format!("memory_failed: {}: {e}", path.display()))?;
    file.lock()
        .map_err(|e| format!("memory_failed: {}: {e}", path.display()))?;
    Ok(file)
}

fn write(path: &Path, text: &str) -> Result<(), String> {
    crate::trigger::replace_mode(path, text.as_bytes(), 0o600)
        .map_err(|e| format!("memory_failed: {e}"))
}

/// Every fact in the folder, sorted by name, or the first file that is not
/// one: a fact the index would leave out is refused, not skipped.
fn facts(dir: &Path) -> Result<Vec<Fact>, String> {
    let failed = |e: std::io::Error| format!("memory_failed: {}: {e}", dir.display());
    let entries = match std::fs::read_dir(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        entries => entries.map_err(failed)?,
    };
    let mut found = Vec::new();
    for (n, entry) in entries.enumerate() {
        if n == MAX_ENTRIES {
            return Err(format!(
                "memory_invalid: {}: more than {MAX_ENTRIES} entries",
                dir.display()
            ));
        }
        let entry = entry.map_err(failed)?;
        let file = entry.file_name().to_string_lossy().into_owned();
        if file == INDEX || file.starts_with('.') || !file.ends_with(".md") || entry.path().is_dir()
        {
            continue;
        }
        let path = entry.path();
        let invalid = |why: String| format!("memory_invalid: {}: {why}", path.display());
        let mut text = String::new();
        std::fs::File::open(&path)
            .and_then(|f| f.take(LIMIT as u64 + 1).read_to_string(&mut text))
            .map_err(|e| invalid(e.to_string()))?;
        if text.len() > LIMIT {
            return Err(invalid(format!("larger than {LIMIT} bytes")));
        }
        found.push(Fact::parse(&file, &text).map_err(invalid)?);
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(found)
}

fn render_index(dir: &Path, facts: &[Fact]) -> String {
    let mut text = format!("# Memory in {}\n\n", dir.display());
    for fact in facts {
        text.push_str(&fact.line());
    }
    text
}

fn save(dir: &Path, fact: &Fact, body: &str) -> Result<Value, String> {
    fact.valid()?;
    if body.trim().is_empty() {
        return Err("invalid_text: a fact needs a body".into());
    }
    let text = fact.render(body);
    if text.len() > LIMIT {
        return Err(format!(
            "fact_too_large: {} bytes with its front matter, above {LIMIT}; save one fact per file",
            text.len()
        ));
    }
    let _lock = lock(dir)?;
    let path = dir.join(format!("{}.md", fact.name));
    let before = std::fs::read_to_string(&path).ok();
    // Index the folder as it will be before writing anything, so a save
    // that does not fit changes nothing.
    let mut all = facts(dir)?;
    all.retain(|other| other.name != fact.name);
    let at = all.partition_point(|other| other.name < fact.name);
    all.insert(at, fact.clone());
    let index = render_index(dir, &all);
    let duplicate = before.as_deref() == Some(text.as_str());
    if !duplicate && index.len() > LIMIT {
        return Err(format!(
            "memory_full: the index would be {} bytes, above {LIMIT}; merge facts or remove one with memory rm NAME first",
            index.len()
        ));
    }
    if !duplicate {
        write(&path, &text)?;
    }
    write(&dir.join(INDEX), &index)?;
    Ok(
        json!({"saved": fact.name, "path": path, "replaced": before.is_some(),
        "duplicate": duplicate, "index": dir.join(INDEX), "index_bytes": index.len(),
        "facts": all.len()}),
    )
}

fn remove(dir: &Path, name: &str) -> Result<Value, String> {
    valid_name(name)?;
    let _lock = lock(dir)?;
    let path = dir.join(format!("{name}.md"));
    // Read the folder first, so one that is not all facts changes nothing.
    let mut all = facts(dir)?;
    all.retain(|fact| fact.name != name);
    let index = render_index(dir, &all);
    let duplicate = match std::fs::remove_file(&path) {
        Ok(()) => false,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => return Err(format!("memory_failed: {}: {e}", path.display())),
    };
    write(&dir.join(INDEX), &index)?;
    Ok(
        json!({"removed": name, "duplicate": duplicate, "index_bytes": index.len(), "facts": all.len()}),
    )
}

/// What is wrong in a folder, without changing it: the first file that is
/// not a fact, an index past the limit, or one that is out of date.
fn check(dir: &Path) -> Value {
    let index = dir.join(INDEX);
    let have = std::fs::read_to_string(&index).ok();
    let bytes = have.as_ref().map_or(0, String::len);
    let mut problems = Vec::new();
    let mut count = 0;
    match facts(dir) {
        Err(error) => problems.push(json!({"problem": error})),
        Ok(all) => {
            count = all.len();
            if have.as_deref() != Some(render_index(dir, &all).as_str())
                && (count > 0 || have.is_some())
            {
                problems.push(json!({"problem": format!("{}: out of date; memory index rewrites it", index.display())}));
            }
        }
    }
    if bytes > LIMIT {
        problems
            .push(json!({"problem": format!("{}: larger than {LIMIT} bytes", index.display())}));
    }
    json!({"dir": dir, "facts": count, "index_bytes": bytes, "limit": LIMIT, "problems": problems})
}

/// Cleanup: a nightly agent merges duplicate facts and drops superseded
/// ones, as the `memory-cleanup` skill says, between `cleanup start` and
/// `cleanup finish`. `~/.agents/memory` is a local git repository, never
/// pushed, so every earlier version is kept: `start` commits what agents
/// saved since, and `finish` commits the cleanup, or reverts it when it
/// removed more facts than the loss guard allows.
const CLEANUP: &str = ".cleanup";
const IGNORE: &str = ".lock\n/.cleanup\n.DS_Store\n";

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        // The person's own git settings must not sign, run hooks or fail
        // for want of a name here.
        .args([
            "-c",
            "user.name=Agent memory",
            "-c",
            "user.email=memory@localhost",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("memory_failed: git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "memory_failed: git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The memory folder as a git repository, made one if it is not yet.
fn repo(root: &Path) -> Result<(), String> {
    std::fs::create_dir_all(root).map_err(|e| format!("memory_failed: {}: {e}", root.display()))?;
    if !root.join(".git").exists() {
        git(root, &["init", "-q"])?;
    }
    let ignore = root.join(".gitignore");
    if std::fs::read_to_string(&ignore).ok().as_deref() != Some(IGNORE) {
        write(&ignore, IGNORE)?;
    }
    Ok(())
}

/// Facts in every scope: the person's and each project's.
fn all_facts(root: &Path) -> Result<usize, String> {
    let mut count = facts(root)?.len();
    let projects = root.join("projects");
    let entries = match std::fs::read_dir(&projects) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(count),
        entries => entries.map_err(|e| format!("memory_failed: {}: {e}", projects.display()))?,
    };
    for entry in entries {
        let entry = entry.map_err(|e| format!("memory_failed: {}: {e}", projects.display()))?;
        if entry.path().is_dir() {
            count += facts(&entry.path())?.len();
        }
    }
    Ok(count)
}

/// Whether anything was saved or removed since the last cleanup, for a
/// trigger's `--if`: exit 0 when so, else `unchanged` and exit 1.
fn changed(root: &Path) -> Result<Value, String> {
    let saved = if root.join(".git").exists() {
        !git(root, &["status", "--porcelain", "--untracked-files=all"])?
            .trim()
            .is_empty()
    } else {
        all_facts(root)? > 0
    };
    if !saved {
        return Err("unchanged: nothing saved or removed since the last cleanup".into());
    }
    Ok(json!({"changed": true}))
}

/// Commit what agents saved since the last cleanup and remember where the
/// cleanup starts. A start while one is open returns that one, so a
/// cleanup cut short is measured from where it began.
fn cleanup_start(root: &Path) -> Result<Value, String> {
    repo(root)?;
    let state = root.join(CLEANUP);
    if let Ok(text) = std::fs::read_to_string(&state) {
        let open: Value = serde_json::from_str(&text)
            .map_err(|e| format!("memory_invalid: {}: {e}", state.display()))?;
        let (Some(commit), Some(facts)) = (open["commit"].as_str(), open["facts"].as_u64()) else {
            return Err(format!(
                "memory_invalid: {}: no commit or facts",
                state.display()
            ));
        };
        return started(root, commit, facts, true);
    }
    let count = all_facts(root)?;
    git(root, &["add", "-A"])?;
    let message = format!("Memory before cleanup, {}", today());
    git(root, &["commit", "-q", "--allow-empty", "-m", &message])?;
    let commit = git(root, &["rev-parse", "HEAD"])?.trim().to_owned();
    write(
        &state,
        &json!({"commit": commit, "facts": count}).to_string(),
    )?;
    started(root, &commit, count as u64, false)
}

/// A started cleanup, with the facts saved and removed since the last one,
/// which are what it reads first.
fn started(root: &Path, commit: &str, facts: u64, duplicate: bool) -> Result<Value, String> {
    let since = git(
        root,
        &["show", "--name-status", "--no-renames", "--format=", commit],
    )?;
    let (removed, added, changed) = fact_changes(&since);
    let saved: Vec<_> = added.into_iter().chain(changed).collect();
    Ok(
        json!({"started": commit, "facts": facts, "saved": saved, "removed": removed,
        "duplicate": duplicate}),
    )
}

/// Fact files in a `--name-status` listing: removed, added and changed.
fn fact_changes(listing: &str) -> (Vec<String>, Vec<String>, Vec<String>) {
    let (mut removed, mut added, mut changed) = (Vec::new(), Vec::new(), Vec::new());
    for line in listing.lines() {
        let Some((status, path)) = line.split_once('\t') else {
            continue;
        };
        let file = Path::new(path)
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("");
        if file == INDEX || !file.ends_with(".md") {
            continue;
        }
        match status {
            "D" => removed.push(path.to_owned()),
            "A" => added.push(path.to_owned()),
            _ => changed.push(path.to_owned()),
        }
    }
    (removed, added, changed)
}

/// Commit the cleanup, or revert it when it removed more than a quarter of
/// the facts it started with (two at least), OpenClaw's guard. Either way
/// git keeps both versions.
fn cleanup_finish(root: &Path) -> Result<Value, String> {
    let state = root.join(CLEANUP);
    let text = match std::fs::read_to_string(&state) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err("cleanup_not_started: run memory cleanup start first".into());
        }
        text => text.map_err(|e| format!("memory_failed: {}: {e}", state.display()))?,
    };
    let open: Value = serde_json::from_str(&text)
        .map_err(|e| format!("memory_invalid: {}: {e}", state.display()))?;
    let (Some(start), Some(before)) = (open["commit"].as_str(), open["facts"].as_u64()) else {
        return Err(format!(
            "memory_invalid: {}: no commit or facts",
            state.display()
        ));
    };
    // A file that is not a fact keeps the cleanup open until it is fixed.
    let after = all_facts(root)?;
    git(root, &["add", "-A"])?;
    let message = format!("Memory cleanup, {}", today());
    git(root, &["commit", "-q", "--allow-empty", "-m", &message])?;
    let head = git(root, &["rev-parse", "HEAD"])?.trim().to_owned();
    let diff = git(
        root,
        &["diff", "--name-status", "--no-renames", start, &head],
    )?;
    let (removed, added, changed) = fact_changes(&diff);
    let limit = (before as usize / 4).max(2);
    if removed.len() > limit {
        git(root, &["revert", "--no-edit", &head])?;
        let _ = std::fs::remove_file(&state);
        return Err(format!(
            "loss_guard: the cleanup removed {} of {before} facts, above {limit}; memory is back as it was at {start}, and the refused cleanup is commit {head}",
            removed.len()
        ));
    }
    std::fs::remove_file(&state).map_err(|e| format!("memory_failed: {}: {e}", state.display()))?;
    Ok(
        json!({"commit": head, "facts": after, "removed": removed, "added": added, "changed": changed}),
    )
}

/// The trigger that runs the cleanup each night: an agent of its own,
/// started on the first fire in the memory folder, woken only when memory
/// changed, and each turn capped.
fn schedule_args(script: &Path, args: &[String]) -> Result<Vec<String>, String> {
    let usage = || format!("usage: {USAGE}");
    let (mut model, mut effort, mut cron) = (None, None, None);
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let slot = match flag.as_str() {
            "--model" => &mut model,
            "--effort" => &mut effort,
            "--cron" => &mut cron,
            _ => return Err(usage()),
        };
        if slot.is_some() {
            return Err(usage());
        }
        *slot = Some(it.next().ok_or_else(usage)?.clone());
    }
    let model = model.ok_or_else(usage)?;
    let quote = format!("'{}'", script.to_string_lossy().replace('\'', r"'\''"));
    let mut add = vec![
        "--name".into(),
        "memory-cleanup".into(),
        "--cron".into(),
        cron.unwrap_or_else(|| "30 3 * * *".into()),
        "--start".into(),
        "memory-cleanup".into(),
        "--model".into(),
        model,
    ];
    if let Some(effort) = effort {
        add.extend(["--effort".into(), effort]);
    }
    add.extend([
        "--if".into(),
        format!("{quote} changed"),
        "--turn-budget-tokens".into(),
        "1000000".into(),
        "--".into(),
        "Clean up memory as the memory-cleanup skill says.".into(),
    ]);
    Ok(add)
}

fn schedule(home: &Path, args: &[String]) -> Result<Value, String> {
    let add = schedule_args(&home.join(".agent/memory"), args)?;
    // The agent the trigger starts works in the memory folder.
    let root = home.join(".agents/memory");
    repo(&root)?;
    std::env::set_current_dir(&root)
        .map_err(|e| format!("memory_failed: {}: {e}", root.display()))?;
    crate::trigger::add_here(&add)
}

/// `~/.agent/memory`, written again whenever the app starts from
/// somewhere else.
pub fn write_script(state: &Path, app: &Path) -> Result<(), String> {
    let quote = |p: &Path| format!("'{}'", p.to_string_lossy().replace('\'', r"'\''"));
    let usage = USAGE.replace('\n', "\n# ");
    let text = format!("#!/bin/sh\n# {usage}\nexec {} {FLAG} \"$@\"\n", quote(app));
    let path = state.join("memory");
    use std::os::unix::fs::PermissionsExt;
    let runnable = std::fs::metadata(&path).is_ok_and(|m| m.permissions().mode() & 0o777 == 0o755);
    if runnable && std::fs::read_to_string(&path).is_ok_and(|have| have == text) {
        return Ok(());
    }
    crate::trigger::replace_mode(&path, text.as_bytes(), 0o755)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agent-memory-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    fn args(line: &[&str]) -> Vec<String> {
        line.iter().map(|a| a.to_string()).collect()
    }

    fn no_stdin() -> Result<String, String> {
        panic!("stdin read")
    }

    fn save_user(root: &Path, name: &str, description: &str, text: &str) -> Result<Value, String> {
        run(
            root,
            root,
            &args(&[
                "save",
                name,
                "--user",
                "--type",
                "feedback",
                "--description",
                description,
                "--source",
                "George, 2026-10-10",
                "--",
                text,
            ]),
            &no_stdin,
        )
    }

    #[test]
    fn a_save_writes_the_fact_and_the_index_and_a_resend_changes_nothing() {
        let root = temp("save");
        let saved = save_user(
            &root,
            "short-replies",
            "George wants one-line answers",
            "Keep replies short.",
        )
        .unwrap();
        assert_eq!(saved["duplicate"], false);
        assert_eq!(saved["replaced"], false);
        assert_eq!(saved["facts"], 1);
        let fact = std::fs::read_to_string(root.join("short-replies.md")).unwrap();
        assert!(fact.starts_with("---\nname: short-replies\ndescription: George wants one-line answers\ntype: feedback\nsource: George, 2026-10-10\nverified: "));
        assert!(fact.ends_with("---\n\nKeep replies short.\n"));
        let index = std::fs::read_to_string(root.join(INDEX)).unwrap();
        assert_eq!(index.len(), saved["index_bytes"].as_u64().unwrap() as usize);
        assert!(index.starts_with(&format!(
            "# Memory in {}\n\n- short-replies (feedback, verified ",
            root.display()
        )));
        assert!(index.ends_with("): George wants one-line answers\n"));
        let again = save_user(
            &root,
            "short-replies",
            "George wants one-line answers",
            "Keep replies short.",
        )
        .unwrap();
        assert_eq!(again["duplicate"], true);
        assert_eq!(again["index_bytes"], saved["index_bytes"]);
        // A save under the same name replaces the fact; the index follows.
        let changed = save_user(
            &root,
            "short-replies",
            "George wants short answers",
            "Two lines at most.",
        )
        .unwrap();
        assert_eq!(
            (changed["duplicate"].clone(), changed["replaced"].clone()),
            (json!(false), json!(true))
        );
        save_user(&root, "a-first", "sorts first", "x").unwrap();
        let index = std::fs::read_to_string(root.join(INDEX)).unwrap();
        let lines: Vec<_> = index.lines().skip(2).collect();
        assert!(
            lines[0].starts_with("- a-first ") && lines[1].ends_with("George wants short answers"),
            "{lines:?}"
        );
        assert_eq!(check(&root)["problems"], json!([]));
        let removed = run(&root, &root, &args(&["rm", "a-first", "--user"]), &no_stdin).unwrap();
        assert_eq!(
            (removed["duplicate"].clone(), removed["facts"].clone()),
            (json!(false), json!(1))
        );
        let again = run(&root, &root, &args(&["rm", "a-first", "--user"]), &no_stdin).unwrap();
        assert_eq!(again["duplicate"], true);
        assert!(
            !std::fs::read_to_string(root.join(INDEX))
                .unwrap()
                .contains("a-first")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_save_past_a_limit_is_refused_and_changes_nothing() {
        let root = temp("limits");
        let code = |r: Result<Value, String>| error_json(&r.unwrap_err())["error"].clone();
        assert_eq!(
            code(save_user(&root, "big", "d", &"x".repeat(LIMIT))),
            "fact_too_large"
        );
        assert_eq!(code(save_user(&root, "Big", "d", "x")), "invalid_name");
        assert_eq!(code(save_user(&root, "memory", "d", "x")), "invalid_name");
        let up = run(&root, &root, &args(&["rm", "../x", "--user"]), &no_stdin);
        assert_eq!(code(up), "invalid_name");
        assert_eq!(
            code(save_user(&root, "two", "a\nb", "x")),
            "invalid_description"
        );
        assert_eq!(code(save_user(&root, "empty", "d", " ")), "invalid_text");
        let wrong_type = run(
            &root,
            &root,
            &args(&[
                "save",
                "t",
                "--user",
                "--type",
                "note",
                "--description",
                "d",
                "--source",
                "s",
                "--",
                "x",
            ]),
            &no_stdin,
        );
        assert_eq!(code(wrong_type), "invalid_type");
        let mut n = 0;
        let full = loop {
            match save_user(
                &root,
                &format!("fact-{n:03}"),
                "a description of some length ".repeat(3).trim(),
                "x",
            ) {
                Ok(_) => n += 1,
                Err(error) => break error,
            }
        };
        assert!(
            full.starts_with("memory_full: the index would be "),
            "{full}"
        );
        let index = std::fs::read_to_string(root.join(INDEX)).unwrap();
        assert!(index.len() <= LIMIT && !index.contains(&format!("fact-{n:03}")));
        assert!(!root.join(format!("fact-{n:03}.md")).exists());
        // Replacing a fact with one no longer still fits.
        assert!(save_user(&root, "fact-000", "shorter", "x").is_ok());
        // A rebuild that would pass the limit is refused too.
        let fact = std::fs::read_to_string(root.join("fact-001.md")).unwrap();
        for extra in n..n + 4 {
            std::fs::write(
                root.join(format!("fact-{extra:03}.md")),
                fact.replace("fact-001", &format!("fact-{extra:03}")),
            )
            .unwrap();
        }
        let before = std::fs::read_to_string(root.join(INDEX)).unwrap();
        let rebuilt = run(&root, &root, &args(&["index", "--user"]), &no_stdin);
        assert_eq!(code(rebuilt), "memory_full");
        assert_eq!(std::fs::read_to_string(root.join(INDEX)).unwrap(), before);
        // Text read from stdin past the limit is refused, not cut short.
        let long = format!("x{}y", " ".repeat(LIMIT));
        let piped = run(
            &root,
            &root,
            &args(&[
                "save",
                "piped",
                "--user",
                "--type",
                "user",
                "--description",
                "d",
                "--source",
                "s",
                "--",
                "-",
            ]),
            &|| Ok(long[..=LIMIT].to_owned()),
        );
        assert_eq!(code(piped), "fact_too_large");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_file_that_is_not_a_fact_is_named_not_skipped() {
        let root = temp("invalid");
        save_user(&root, "good", "a good fact", "x").unwrap();
        std::fs::write(root.join("notes.md"), "no front matter").unwrap();
        let error = save_user(&root, "other", "another", "x").unwrap_err();
        assert!(
            error.starts_with(&format!(
                "memory_invalid: {}: no front matter",
                root.join("notes.md").display()
            )),
            "{error}"
        );
        assert!(!root.join("other.md").exists());
        // A removal in such a folder changes nothing either.
        let removed = run(&root, &root, &args(&["rm", "good", "--user"]), &no_stdin);
        assert!(removed.unwrap_err().starts_with("memory_invalid: "));
        assert!(root.join("good.md").exists());
        let report = check(&root);
        assert_eq!(report["problems"].as_array().unwrap().len(), 1, "{report}");
        std::fs::write(root.join("notes.md"), "---\nname: elsewhere\ndescription: d\ntype: user\nsource: s\nverified: 2026-10-10\n---\n\nx\n").unwrap();
        assert!(
            save_user(&root, "other", "another", "x")
                .unwrap_err()
                .contains("its name is elsewhere")
        );
        std::fs::remove_file(root.join("notes.md")).unwrap();
        // Other files and folders are not facts.
        std::fs::write(root.join("draft.txt"), "x").unwrap();
        std::fs::create_dir_all(root.join("projects/demo")).unwrap();
        // A hand edit leaves the index out of date until it is rebuilt.
        let fact = std::fs::read_to_string(root.join("good.md")).unwrap();
        std::fs::write(
            root.join("good.md"),
            fact.replace("a good fact", "an edited fact"),
        )
        .unwrap();
        assert!(
            check(&root)["problems"][0]["problem"]
                .as_str()
                .unwrap()
                .ends_with("out of date; memory index rewrites it")
        );
        let rebuilt = run(&root, &root, &args(&["index", "--user"]), &no_stdin).unwrap();
        assert_eq!(rebuilt["facts"], 1);
        assert_eq!(check(&root)["problems"], json!([]));
        assert_eq!(check(&root.join("none"))["problems"], json!([]));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_scope_is_the_person_a_named_project_or_this_folders_project() {
        let root = temp("scope-root");
        let repo = temp("scope-repo");
        let git = |dir: &Path, line: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(line)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{line:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&repo, &["init", "-q"]);
        std::fs::create_dir_all(repo.join("web/.agents")).unwrap();
        std::fs::write(repo.join("web/.agents/project.toml"), "name = \"demo\"\n").unwrap();
        std::fs::write(repo.join("web/index.html"), "x").unwrap();
        // The project's file is not committed, as when the app made it.
        git(&repo, &["add", "web/index.html"]);
        git(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "x",
            ],
        );
        let worktree = repo.with_extension("worktree");
        let _ = std::fs::remove_dir_all(&worktree);
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "agent/demo.task",
                worktree.to_str().unwrap(),
                "HEAD",
            ],
        );
        assert_eq!(project_of(&worktree.join("web")).unwrap(), "demo");
        assert_eq!(project_of(&repo.join("web")).unwrap(), "demo");
        let unknown = project_of(&repo).unwrap_err();
        assert!(unknown.starts_with("project_unknown: "), "{unknown}");
        let save = |cwd: &Path, scope: &[&str]| {
            let mut line = vec![
                "save",
                "x",
                "--type",
                "project",
                "--description",
                "d",
                "--source",
                "s",
            ];
            line.extend(scope);
            line.extend(["--", "-"]);
            run(&root, cwd, &args(&line), &|| Ok("from stdin".into()))
        };
        assert_eq!(
            save(&worktree.join("web"), &[]).unwrap()["path"],
            json!(root.join("projects/demo/x.md"))
        );
        assert_eq!(
            save(&repo, &["--project", "other"]).unwrap()["path"],
            json!(root.join("projects/other/x.md"))
        );
        assert_eq!(
            save(&repo, &["--user"]).unwrap()["path"],
            json!(root.join("x.md"))
        );
        assert!(
            std::fs::read_to_string(root.join("x.md"))
                .unwrap()
                .ends_with("\n\nfrom stdin\n")
        );
        // show gives the person's index and this folder's project's.
        let show = |cwd: &Path, scope: &[&str]| {
            let mut line = vec!["show"];
            line.extend(scope);
            run(&root, cwd, &args(&line), &no_stdin)
        };
        let shown = show(&worktree.join("web"), &[]).unwrap();
        assert_eq!(shown["user"]["dir"], json!(root));
        assert!(
            shown["user"]["index"]
                .as_str()
                .unwrap()
                .contains("\n- x (project, verified ")
        );
        assert_eq!(shown["project"]["name"], "demo");
        assert_eq!(shown["project"]["dir"], json!(root.join("projects/demo")));
        assert!(
            shown["project"]["index"]
                .as_str()
                .unwrap()
                .starts_with("# Memory in ")
        );
        assert_eq!(show(&repo, &[]).unwrap()["project"], json!(null));
        assert_eq!(
            show(&worktree, &["--user"]).unwrap()["project"],
            json!(null)
        );
        assert_eq!(
            show(&repo, &["--project", "none"]).unwrap()["project"]["index"],
            ""
        );
        assert!(
            show(&repo, &["--project", "../up"])
                .unwrap_err()
                .starts_with("invalid_project: ")
        );
        assert!(
            save(&repo, &["--project", "../up"])
                .unwrap_err()
                .starts_with("invalid_project: ")
        );
        assert!(
            save(&repo, &["--user", "--project", "p"])
                .unwrap_err()
                .starts_with("usage: ")
        );
        for line in [
            &["save"][..],
            &["rm"],
            &["index", "x"],
            &["check", "--type", "user"],
            &["show", "x"],
            &["list"],
            &[],
        ] {
            assert!(
                run(&root, &repo, &args(line), &no_stdin)
                    .unwrap_err()
                    .starts_with("usage: "),
                "{line:?}"
            );
        }
        git(
            &repo,
            &["worktree", "remove", "--force", worktree.to_str().unwrap()],
        );
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(repo).unwrap();
    }

    #[test]
    fn a_cleanup_is_committed_or_reverted_by_the_loss_guard() {
        let root = temp("cleanup");
        let code = |r: Result<Value, String>| error_json(&r.unwrap_err())["error"].clone();
        let run_ = |line: &[&str]| run(&root, &root, &args(line), &no_stdin);
        assert_eq!(code(changed(&root)), "unchanged");
        for name in ["a", "b", "c", "d"] {
            save_user(&root, name, &format!("fact {name}"), "x").unwrap();
        }
        assert_eq!(changed(&root).unwrap(), json!({"changed": true}));
        assert_eq!(code(run_(&["cleanup", "finish"])), "cleanup_not_started");
        let started = run_(&["cleanup", "start"]).unwrap();
        assert_eq!(
            (started["facts"].clone(), started["duplicate"].clone()),
            (json!(4), json!(false))
        );
        assert_eq!(started["saved"], json!(["a.md", "b.md", "c.md", "d.md"]));
        // A start while one is open is that one, so the guard counts from it.
        let again = run_(&["cleanup", "start"]).unwrap();
        assert_eq!(
            (again["started"].clone(), again["duplicate"].clone()),
            (started["started"].clone(), json!(true))
        );
        assert_eq!(again["saved"], started["saved"]);
        assert_eq!(code(changed(&root)), "unchanged");
        // Merge two facts into one.
        run_(&["rm", "b", "--user"]).unwrap();
        save_user(&root, "a", "facts a and b", "x and y").unwrap();
        let done = run_(&["cleanup", "finish"]).unwrap();
        assert_eq!(done["removed"], json!(["b.md"]));
        assert_eq!(done["changed"], json!(["a.md"]));
        assert_eq!(done["facts"], 3);
        assert_eq!(code(changed(&root)), "unchanged");
        // Removing more than the guard allows is undone.
        save_user(&root, "e", "fact e", "x").unwrap();
        let next = run_(&["cleanup", "start"]).unwrap();
        assert_eq!(
            (next["saved"].clone(), next["removed"].clone()),
            (json!(["e.md"]), json!([]))
        );
        for name in ["a", "c", "d"] {
            run_(&["rm", name, "--user"]).unwrap();
        }
        let refused = run_(&["cleanup", "finish"]).unwrap_err();
        assert!(
            refused.starts_with("loss_guard: the cleanup removed 3 of 4 facts, above 2"),
            "{refused}"
        );
        for name in ["a", "c", "d", "e"] {
            assert!(root.join(format!("{name}.md")).exists(), "{name}");
        }
        assert!(
            std::fs::read_to_string(root.join(INDEX))
                .unwrap()
                .contains("- d (feedback")
        );
        assert_eq!(check(&root)["problems"], json!([]));
        assert_eq!(code(changed(&root)), "unchanged");
        assert!(!root.join(CLEANUP).exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_schedule_is_a_gated_capped_trigger_that_starts_its_own_agent() {
        let script = Path::new("/Users/g/.agent/memory");
        let line = |rest: &[&str]| schedule_args(script, &args(rest));
        let add = line(&["--model", "openai/mini"]).unwrap();
        assert_eq!(
            add,
            args(&[
                "--name",
                "memory-cleanup",
                "--cron",
                "30 3 * * *",
                "--start",
                "memory-cleanup",
                "--model",
                "openai/mini",
                "--if",
                "'/Users/g/.agent/memory' changed",
                "--turn-budget-tokens",
                "1000000",
                "--",
                "Clean up memory as the memory-cleanup skill says.",
            ])
        );
        let add = line(&["--model", "m", "--effort", "low", "--cron", "0 4 * * *"]).unwrap();
        assert_eq!(add[3], "0 4 * * *");
        assert_eq!(add[8..10], args(&["--effort", "low"]));
        for bad in [
            &[][..],
            &["--effort", "low"],
            &["--model"],
            &["--model", "m", "--model", "n"],
            &["--every", "1d"],
        ] {
            assert!(line(bad).unwrap_err().starts_with("usage: "), "{bad:?}");
        }
    }

    #[test]
    fn the_script_runs_the_app_with_the_memory_flag() {
        let state = temp("script");
        write_script(
            &state,
            Path::new("/Apps/Agent's.app/Contents/MacOS/agent-app"),
        )
        .unwrap();
        let text = std::fs::read_to_string(state.join("memory")).unwrap();
        assert!(
            text.starts_with("#!/bin/sh\n# usage: memory show [SCOPE]\n#        memory save NAME")
        );
        assert!(text.ends_with(
            "\nexec '/Apps/Agent'\\''s.app/Contents/MacOS/agent-app' --memory \"$@\"\n"
        ));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(state.join("memory"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        std::fs::remove_dir_all(state).unwrap();
    }
}
