//! Memory: facts agents save for later agents, one Markdown file each. The
//! person's are in `~/.agents/memory`; a project's are in
//! `~/.agents/memory/projects/NAME`, outside its checkout, so its lead and
//! every task's worktree share one copy whatever is committed. Each folder's
//! `MEMORY.md` is an index generated from the facts' front matter and kept
//! within 16 KiB, so reading it at the start of every task stays cheap; a
//! save that would pass that is refused until facts are merged or removed.
//! The daemon knows nothing of memory.
//!
//! `~/.agent/memory`, a script Sonny writes, shows the indexes, reads,
//! saves, removes, indexes and checks facts; the `memory` skill Sonny
//! installs says what to save. A read is recorded beside the facts, so the
//! cleanup can find facts no agent has opened in a while. `schedule` adds a nightly trigger, through
//! `~/.agent/trigger`, whose agent follows the `memory-cleanup` skill
//! between `cleanup start` and `cleanup finish`, which commit to a local
//! git repository in `~/.agents/memory` and revert a cleanup that removed too
//! many facts, keeping what other agents saved while it ran; `changed` gates
//! the trigger so a quiet night costs no turn.
use serde_json::{Value, json};
use std::{
    io::Read,
    path::{Path, PathBuf},
};

/// A fact file, front matter included, and a folder's index: each at most
/// this. Bounded, so every task can read the index whole, and roomy enough
/// that a busy project's leads and tasks do not spend turns trimming it.
const LIMIT: usize = 16 * 1024;
/// Entries a folder may hold, facts or not, before it is refused as not a
/// memory folder.
const MAX_ENTRIES: usize = 1024;
const INDEX: &str = "MEMORY.md";
const TYPES: [&str; 4] = ["user", "feedback", "project", "reference"];
/// Each folder's record of when agents last read each of its facts, and
/// how often, by name. It is kept apart from the facts and out of git, so a
/// read changes no fact, no index, no agent's cached instructions, and no
/// night's `changed`.
const READS: &str = ".reads";
/// Days since a fact was last read or saved after which the cleanup is
/// asked to check that it still holds: Codex drops a memory unused for 30,
/// Copilot one unused for 28.
const UNREAD_DAYS: i64 = 30;
const USAGE: &str = "usage: memory show [SCOPE]\n       memory read NAME [SCOPE]\n       memory save NAME --type user|feedback|project|reference --description TEXT --source TEXT [--external] [--confirmed N] [--pushed-back N] [SCOPE] -- TEXT|-\n       memory rm NAME [SCOPE]\n       memory index [SCOPE]\n       memory check [SCOPE]\n       memory said [SINCE_MS]   (Sonny: what the person wrote to any agent since then)\n       memory changed | cleanup start | cleanup finish\n       memory schedule [--model PROVIDER/MODEL [--effort LEVEL]] [--cron 'MIN HOUR DAY MONTH WEEKDAY']\n         --model starts the memory-cleanup agent; without it, the existing one\n         SCOPE: --user | --project NAME; none: the project of this folder";

/// `memory show|save|rm|index|check|changed|cleanup|schedule`, as
/// `~/.agent/memory` runs it.
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

/// A `memory` call of an agent working in `cwd`, as a client's tool takes
/// it: the script's commands but `schedule`, which is the person's, with
/// the fact's text in the call. `read` NAME, `save {name, type,
/// description, source, text, external}`, `remove` NAME, `index`, `check`, `changed`
/// or `cleanup` (`start` or `finish`); with none, `show`. `user` or `project` picks the scope.
pub fn call(home: &Path, cwd: &Path, arguments: &Value) -> Result<Value, String> {
    let text = |value: &Value| value.as_str().unwrap_or_default().to_owned();
    let mut scope = Vec::new();
    if arguments["user"] == true {
        scope.push("--user".to_owned());
    } else if let Some(project) = arguments["project"].as_str() {
        scope.extend(["--project".to_owned(), project.to_owned()]);
    }
    let save = &arguments["save"];
    let mut args: Vec<String> = if save.is_object() {
        let mut args = vec!["save".to_owned(), text(&save["name"])];
        for (key, flag) in [
            ("type", "--type"),
            ("description", "--description"),
            ("source", "--source"),
        ] {
            args.extend([flag.to_owned(), text(&save[key])]);
        }
        if save["external"] == true {
            args.push("--external".to_owned());
        }
        for (key, flag) in [
            ("confirmed", "--confirmed"),
            ("pushed_back", "--pushed-back"),
        ] {
            if let Some(n) = save[key].as_u64() {
                args.extend([flag.to_owned(), n.to_string()]);
            }
        }
        args
    } else if let Some(name) = arguments["read"].as_str() {
        vec!["read".to_owned(), name.to_owned()]
    } else if let Some(name) = arguments["remove"].as_str() {
        vec!["rm".to_owned(), name.to_owned()]
    } else if let Some(step) = arguments["cleanup"].as_str() {
        vec!["cleanup".to_owned(), step.to_owned()]
    } else if arguments["changed"] == true {
        vec!["changed".to_owned()]
    } else if arguments["index"] == true {
        vec!["index".to_owned()]
    } else if arguments["check"] == true {
        vec!["check".to_owned()]
    } else {
        vec!["show".to_owned()]
    };
    if !matches!(args[0].as_str(), "cleanup" | "changed") {
        args.extend(scope);
    }
    if save.is_object() {
        args.extend(["--".to_owned(), text(&save["text"])]);
    }
    // The text is the call's own: a lone "-" is that text, not stdin.
    run(&home.join(".agents/memory"), cwd, &args, &|| {
        Ok("-".to_owned())
    })
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
    // Bot ids are each store's own, and stores under one HOME share its
    // memory: the store names which bot.
    let caller = std::env::var("AGENT_BOT_ID")
        .ok()
        .map(|id| match std::env::var("AGENT_STORE") {
            Ok(store) => format!("{store}#{id}"),
            Err(_) => id,
        });
    run_as(root, cwd, args, stdin, caller.as_deref())
}

/// [`run`] for `caller`, the agent's id, or none for the person.
fn run_as(
    root: &Path,
    cwd: &Path,
    args: &[String],
    stdin: &dyn Fn() -> Result<String, String>,
    caller: Option<&str>,
) -> Result<Value, String> {
    let usage = || format!("usage: {USAGE}");
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["changed"] => return changed(root),
        ["cleanup", "start"] => return cleanup_start(root, caller),
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
    let mut external = false;
    let mut counts = [None, None];
    let mut it = flags.iter();
    while let Some(arg) = it.next() {
        let slot = match arg.as_str() {
            "--external" if verb == "save" && !external => {
                external = true;
                continue;
            }
            count @ ("--confirmed" | "--pushed-back") if verb == "save" => {
                let at = usize::from(count == "--pushed-back");
                let n = it.next().and_then(|n| n.parse::<u32>().ok());
                if counts[at].is_some() || n.is_none() {
                    return Err(usage());
                }
                counts[at] = n;
                continue;
            }
            "--type" => &mut fields[0],
            "--description" => &mut fields[1],
            "--source" => &mut fields[2],
            "--project" => &mut scope,
            "--user" if scope.is_none() => {
                scope = Some(String::new());
                continue;
            }
            flag if flag.starts_with('-') => return Err(usage()),
            _ if name.is_none() && matches!(verb.as_str(), "save" | "rm" | "read") => {
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
    let scoped = scope.is_some();
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
                external,
                confirmed: counts[0].unwrap_or_default(),
                pushed_back: counts[1].unwrap_or_default(),
            };
            let dir = scope()?;
            let path = dir.join(format!("{}.md", fact.name));
            save(&dir, fact, counts, &text, &|now| {
                noted(root, caller, &path, now)
            })
        }
        // A name not in the project's facts is looked for in the person's,
        // as `show` lists both: unscoped, a read finds either, and outside
        // a project the person's. A scope given is the only one read.
        ("read", Some(name), [None, None, None], None) if !scoped => match project() {
            Err(error) if error.starts_with("project_unknown: ") => read(root, &name),
            Err(error) => Err(error),
            Ok(None) => read(root, &name),
            Ok(Some(project)) => match read(&root.join("projects").join(project), &name) {
                Err(error) if error.starts_with("fact_not_found: ") => {
                    read(root, &name).map_err(|e| match e.starts_with("fact_not_found: ") {
                        true => format!(
                            "fact_not_found: {name}: no such fact in this project or the person's"
                        ),
                        false => e,
                    })
                }
                found => found,
            },
        },
        ("read", Some(name), [None, None, None], None) => read(&scope()?, &name),
        ("rm", Some(name), [None, None, None], None) => {
            let dir = scope()?;
            let path = dir.join(format!("{name}.md"));
            remove(&dir, &name, &|now| noted(root, caller, &path, now))
        }
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
    match crate::policy::project(cwd) {
        Ok(Some(name)) => Ok(name),
        Ok(None) => Err(format!(
            "project_unknown: {}: no .agents/project.toml at or above this folder; pass --project NAME or --user",
            cwd.display()
        )),
        Err(error) => Err(format!("project_invalid: {error}")),
    }
}

/// The local date, for a fact's `verified` and its last read.
fn today() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    // SAFETY: localtime_r fills the zeroed struct it is given and nothing else.
    let day = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&(now as libc::time_t), &mut tm);
        tm
    };
    format!(
        "{:04}-{:02}-{:02}",
        day.tm_year + 1900,
        day.tm_mon + 1,
        day.tm_mday
    )
}

/// Days from 1970-01-01 to a `YYYY-MM-DD` date, for how long ago a fact
/// was read (Howard Hinnant's `days_from_civil`).
fn day_number(date: &str) -> Option<i64> {
    let mut parts = date.splitn(3, '-').map(|part| part.parse::<i64>().ok());
    let (year, month, day) = (parts.next()??, parts.next()??, parts.next()??);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let of_era = year - era * 400;
    let of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let of_cycle = of_era * 365 + of_era / 4 - of_era / 100 + of_year;
    Some(era * 146_097 + of_cycle - 719_468)
}

#[derive(Debug, Clone, PartialEq)]
struct Fact {
    name: String,
    kind: String,
    description: String,
    source: String,
    verified: String,
    /// Saved in a turn that read outside content, a web page, a search or
    /// an MCP server, so it may carry words no person wrote.
    external: bool,
    /// How many times the person confirmed what the fact says, and pushed
    /// back on it, as ExpeL's insights and ACE's playbook bullets keep
    /// helpful and harmful counts, so a later agent sees its weight.
    confirmed: u32,
    pushed_back: u32,
}

impl Fact {
    fn render(&self, body: &str) -> String {
        format!(
            "---\nname: {}\ndescription: {}\ntype: {}\nsource: {}\nverified: {}\n{}{}---\n\n{}\n",
            self.name,
            self.description,
            self.kind,
            self.source,
            self.verified,
            if self.external {
                "external: true\n"
            } else {
                ""
            },
            self.counts(|key, n| format!("{key}: {n}\n")),
            body.trim()
        )
    }

    /// The counts that are not zero, each as `each` writes it.
    fn counts(&self, each: impl Fn(&str, u32) -> String) -> String {
        [
            ("confirmed", self.confirmed),
            ("pushed_back", self.pushed_back),
        ]
        .into_iter()
        .filter(|(_, n)| *n > 0)
        .map(|(key, n)| each(key, n))
        .collect()
    }

    fn line(&self) -> String {
        format!(
            "- {} ({}, verified {}{}{}): {}\n",
            self.name,
            self.kind,
            self.verified,
            if self.external { ", external" } else { "" },
            self.counts(|key, n| format!(", {} {n}", key.replace('_', " "))),
            self.description
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
            external: match fields.get("external") {
                None => false,
                Some(&"true") => true,
                Some(other) => return Err(format!("external is true or absent, not {other}")),
            },
            confirmed: count(&fields, "confirmed")?,
            pushed_back: count(&fields, "pushed_back")?,
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

/// A count in a fact's front matter: absent is none.
fn count(fields: &std::collections::HashMap<&str, &str>, key: &str) -> Result<u32, String> {
    fields.get(key).map_or(Ok(0), |value| {
        value
            .parse()
            .map_err(|_| format!("{key} is a whole number or absent, not {value}"))
    })
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
    crate::files::replace_mode(path, text.as_bytes(), 0o600)
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

/// Save `fact`; a count left out (`None` in `counts`) keeps the one the
/// fact had, so an agent rewording a fact does not reset its weight.
/// What a save or removal tells the cleanup journal ([`noted`]): the fact
/// as the change left it, while the folder is still locked.
type Note<'a> = &'a dyn Fn(Option<&str>) -> Result<(), String>;

fn save(
    dir: &Path,
    mut fact: Fact,
    counts: [Option<u32>; 2],
    body: &str,
    note: Note,
) -> Result<Value, String> {
    fact.valid()?;
    if body.trim().is_empty() {
        return Err("invalid_text: a fact needs a body".into());
    }
    let _lock = lock(dir)?;
    let path = dir.join(format!("{}.md", fact.name));
    let before = std::fs::read_to_string(&path).ok();
    if let Some(had) =
        (before.as_deref()).and_then(|text| Fact::parse(&format!("{}.md", fact.name), text).ok())
    {
        fact.confirmed = counts[0].unwrap_or(had.confirmed);
        fact.pushed_back = counts[1].unwrap_or(had.pushed_back);
    }
    let text = fact.render(body);
    if text.len() > LIMIT {
        return Err(format!(
            "fact_too_large: {} bytes with its front matter, above {LIMIT}; save one fact per file",
            text.len()
        ));
    }
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
    note(Some(&text))?;
    write(&dir.join(INDEX), &index)?;
    Ok(
        json!({"saved": fact.name, "path": path, "replaced": before.is_some(),
        "duplicate": duplicate, "index": dir.join(INDEX), "index_bytes": index.len(),
        "facts": all.len()}),
    )
}

/// A folder's reads. Reads are bookkeeping, not memory: a record that is
/// missing or unreadable counts as none rather than failing the read.
fn reads(dir: &Path) -> serde_json::Map<String, Value> {
    std::fs::read_to_string(dir.join(READS))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// One fact's whole file, recorded as read today.
fn read(dir: &Path, name: &str) -> Result<Value, String> {
    valid_name(name)?;
    let file = format!("{name}.md");
    let path = dir.join(&file);
    let mut text = String::new();
    match std::fs::File::open(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!(
                "fact_not_found: {name}: no such fact in {}",
                dir.display()
            ));
        }
        opened => opened
            .and_then(|f| f.take(LIMIT as u64 + 1).read_to_string(&mut text))
            .map_err(|e| format!("memory_failed: {}: {e}", path.display()))?,
    };
    let invalid = |why: String| format!("memory_invalid: {}: {why}", path.display());
    if text.len() > LIMIT {
        return Err(invalid(format!("larger than {LIMIT} bytes")));
    }
    Fact::parse(&file, &text).map_err(invalid)?;
    // Locked only to record it, so a read of no fact makes no folder.
    let _lock = lock(dir)?;
    let mut all = reads(dir);
    let count = all
        .get(name)
        .and_then(|read| read["count"].as_u64())
        .unwrap_or(0)
        + 1;
    let last = today();
    all.insert(name.to_owned(), json!({"last": last, "count": count}));
    write(&dir.join(READS), &Value::Object(all).to_string())?;
    Ok(json!({"read": name, "path": path, "text": text, "reads": count}))
}

fn remove(dir: &Path, name: &str, note: Note) -> Result<Value, String> {
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
    note(None)?;
    write(&dir.join(INDEX), &index)?;
    let mut all_reads = reads(dir);
    if all_reads.remove(name).is_some() {
        write(&dir.join(READS), &Value::Object(all_reads).to_string())?;
    }
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
/// pushed, holding memory as it was at each cleanup: `start` commits what agents
/// saved since, and `finish` commits the cleanup, or reverts it when it
/// removed more facts than the loss guard allows.
const CLEANUP: &str = ".cleanup";
/// When the last finished cleanup started, in ms: what the person wrote
/// from then on is the next cleanup's to read.
const SAID: &str = ".said";
/// What other agents saved and removed while a cleanup was open, one JSON
/// line each: the fact's path under the memory folder and its text, null
/// once removed. A cleanup the loss guard refuses puts them back over its
/// revert, so the guard takes back only the cleanup's own changes.
const OTHERS: &str = ".cleanup-others";
const IGNORE: &str = ".lock\n.reads\n/.cleanup\n/.cleanup-others\n/.said\n.DS_Store\n";

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let mut command = std::process::Command::new("git");
    // A hook's `GIT_DIR` and the like would name another repository.
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
    ] {
        command.env_remove(key);
    }
    let out = command
        .arg("-C")
        .arg(root)
        // The person's own git settings must not sign, run hooks, change
        // line endings or fail for want of a name here.
        .args([
            "-c",
            "user.name=Agent memory",
            "-c",
            "user.email=memory@localhost",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.autocrlf=false",
            "-c",
            "core.eol=lf",
            // Nor their ignore or attribute rules: every fact is kept, byte
            // for byte.
            "-c",
            "core.excludesFile=/dev/null",
            "-c",
            "core.attributesFile=/dev/null",
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
    let new = !root.join(".git").exists();
    if new {
        // Objects as private as the facts, which are 0600.
        git(root, &["init", "-q", "--shared=0600"])?;
    }
    let ignore = root.join(".gitignore");
    if std::fs::read_to_string(&ignore).ok().as_deref() != Some(IGNORE) {
        write(&ignore, IGNORE)?;
    }
    if new {
        // Only the ignore file, so a new repository is not a change to
        // clean up, and facts already there still are.
        git(root, &["add", ".gitignore"])?;
        git(root, &["commit", "-q", "-m", "Memory repository"])?;
    }
    Ok(())
}

/// Files git wrote back are made 0600 again, as `write` makes them.
fn private(root: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let listed = git(root, &["ls-files", "-z"])?;
    for path in listed.split('\0').filter(|p| !p.is_empty()) {
        let path = root.join(path);
        match std::fs::symlink_metadata(&path) {
            Ok(m) if m.is_file() && m.permissions().mode() & 0o777 != 0o600 => {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                    .map_err(|e| format!("memory_failed: {}: {e}", path.display()))?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Every scope's folder: the person's, then each project's.
fn scopes(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut dirs = vec![root.to_path_buf()];
    let projects = root.join("projects");
    let entries = match std::fs::read_dir(&projects) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(dirs),
        entries => entries.map_err(|e| format!("memory_failed: {}: {e}", projects.display()))?,
    };
    for entry in entries {
        let entry = entry.map_err(|e| format!("memory_failed: {}: {e}", projects.display()))?;
        if entry.path().is_dir() {
            dirs.push(entry.path());
        }
    }
    Ok(dirs)
}

/// Facts in every scope: the person's and each project's.
fn all_facts(root: &Path) -> Result<usize, String> {
    let mut count = 0;
    for dir in scopes(root)? {
        count += facts(&dir)?.len();
    }
    Ok(count)
}

/// Facts in every scope that no agent has read or saved for
/// [`UNREAD_DAYS`], oldest first, by their path under `root` as a cleanup
/// lists the facts saved since the last one.
fn unread(root: &Path) -> Result<Vec<Value>, String> {
    let Some(now) = day_number(&today()) else {
        return Ok(Vec::new());
    };
    let mut found = Vec::new();
    for dir in scopes(root)? {
        let reads = reads(&dir);
        for fact in facts(&dir)? {
            let last_read = reads
                .get(&fact.name)
                .and_then(|read| read["last"].as_str())
                .map(str::to_owned);
            let Some(last) = [Some(fact.verified.as_str()), last_read.as_deref()]
                .into_iter()
                .flatten()
                .filter_map(day_number)
                .max()
            else {
                continue;
            };
            if now - last >= UNREAD_DAYS {
                let path = dir.join(format!("{}.md", fact.name));
                let path = path.strip_prefix(root).unwrap_or(&path);
                found.push((
                    last,
                    json!({"path": path, "last_read": last_read, "verified": fact.verified}),
                ));
            }
        }
    }
    found.sort_by_key(|(last, _)| *last);
    Ok(found.into_iter().map(|(_, fact)| fact).collect())
}

/// Whether anything was saved or removed since the last cleanup, or a
/// cleanup is open, for a trigger's `--if`: exit 0 when so, else
/// `unchanged` and exit 1.
fn changed(root: &Path) -> Result<Value, String> {
    // A cleanup cut short counts too, so the next night finishes it.
    let saved = if root.join(CLEANUP).exists() {
        true
    } else if root.join(".git").exists() {
        // Reads are not changes, also in a repository whose ignore file
        // predates them.
        git(root, &["status", "--porcelain", "--untracked-files=all"])?
            .lines()
            .any(|line| {
                let file = Path::new(line.get(3..).unwrap_or("")).file_name();
                file != Some(READS.as_ref()) && file != Some(SAID.as_ref())
            })
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
fn cleanup_start(root: &Path, caller: Option<&str>) -> Result<Value, String> {
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
    let at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    match std::fs::remove_file(root.join(OTHERS)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(format!("memory_failed: {OTHERS}: {e}"));
        }
        _ => {}
    }
    write(
        &state,
        &json!({"commit": commit, "facts": count, "at_ms": at_ms, "by": caller}).to_string(),
    )?;
    started(root, &commit, count as u64, false)
}

/// A started cleanup, with the facts saved and removed since the last one,
/// which are what it reads first, those no agent has read in a while,
/// which it checks still hold, and when the last finished one started
/// (`since_ms`, null before the first), from which it reads what the person
/// wrote: a turn that ended while that one ran is read again rather than
/// missed.
fn started(root: &Path, commit: &str, facts: u64, duplicate: bool) -> Result<Value, String> {
    let since = git(
        root,
        &["show", "--name-status", "--no-renames", "--format=", commit],
    )?;
    let (removed, added, changed) = fact_changes(&since);
    let saved: Vec<_> = added.into_iter().chain(changed).collect();
    let since_ms = said_since(root);
    Ok(
        json!({"started": commit, "facts": facts, "saved": saved, "removed": removed,
        "unread": unread(root)?, "since_ms": since_ms, "duplicate": duplicate}),
    )
}

/// When the last finished cleanup started, in unix milliseconds, from
/// which the next reads what the person wrote; `None` before the first.
pub fn said_since(root: &Path) -> Option<i64> {
    std::fs::read_to_string(root.join(SAID))
        .ok()
        .and_then(|ms| ms.trim().parse::<i64>().ok())
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

/// Note a fact another agent saved (`now` its text) or removed (none)
/// while a cleanup is open, for [`put_back`]. The cleanup's own agent's
/// changes are the cleanup's. Called under the fact's folder lock, so the
/// notes are in the order the changes were made.
fn noted(root: &Path, caller: Option<&str>, path: &Path, now: Option<&str>) -> Result<(), String> {
    let Ok(open) = std::fs::read_to_string(root.join(CLEANUP)) else {
        return Ok(());
    };
    let by = serde_json::from_str::<Value>(&open).unwrap_or_default()["by"].clone();
    if by.as_str() == caller {
        return Ok(());
    }
    let Ok(relative) = path.strip_prefix(root) else {
        return Ok(());
    };
    let line = format!("{}\n", json!({"path": relative, "text": now}));
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let others = root.join(OTHERS);
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&others)
        .and_then(|mut file| file.write_all(line.as_bytes()))
        .map_err(|e| format!("memory_failed: {}: {e}", others.display()))
}

/// Put back over a refused cleanup's revert what other agents saved and
/// removed while it was open, each fact as they last left it, reindex
/// their folders and commit them: how many facts, and each folder whose
/// index is now past the limit (the cleanup's removals had made room for
/// saves the revert brings back beside). Nothing saved is dropped to fit.
/// The caller holds every folder's lock.
fn put_back(root: &Path) -> Result<(usize, Vec<PathBuf>), String> {
    let others = root.join(OTHERS);
    let Ok(listed) = std::fs::read_to_string(&others) else {
        return Ok((0, Vec::new()));
    };
    let mut last: Vec<(PathBuf, Option<String>)> = Vec::new();
    for line in listed.lines() {
        let Ok(noted) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(path) = noted["path"].as_str().map(PathBuf::from) else {
            continue;
        };
        // Only a fact under the memory folder, as `noted` wrote it.
        if !path
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
        {
            continue;
        }
        last.retain(|(had, _)| had != &path);
        last.push((path, noted["text"].as_str().map(str::to_owned)));
    }
    let mut dirs: Vec<PathBuf> = Vec::new();
    for (path, text) in &last {
        let path = root.join(path);
        let Some(dir) = path.parent() else { continue };
        match text {
            Some(text) => std::fs::create_dir_all(dir)
                .map_err(|e| format!("memory_failed: {}: {e}", dir.display()))
                .and_then(|()| write(&path, text))?,
            None => match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    return Err(format!("memory_failed: {}: {e}", path.display()));
                }
                _ => {}
            },
        }
        if !dirs.iter().any(|had| had == dir) {
            dirs.push(dir.to_owned());
        }
    }
    let mut full = Vec::new();
    for dir in &dirs {
        let index = render_index(dir, &facts(dir)?);
        if index.len() > LIMIT {
            full.push(dir.clone());
        }
        write(&dir.join(INDEX), &index)?;
    }
    if !last.is_empty() {
        git(root, &["add", "-A"])?;
        let message = format!("Memory saved while a refused cleanup ran, {}", today());
        git(root, &["commit", "-q", "--allow-empty", "-m", &message])?;
    }
    std::fs::remove_file(&others)
        .map_err(|e| format!("memory_failed: {}: {e}", others.display()))?;
    Ok((last.len(), full))
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
    // Every folder stays locked until the cleanup is done, so no save or
    // removal lands between its commit, its revert and the put-back of
    // what others changed: theirs wait, and come after. In one order,
    // since another finish takes them all too.
    let mut dirs = scopes(root)?;
    dirs.sort();
    let _locks = dirs
        .iter()
        .map(|dir| lock(dir))
        .collect::<Result<Vec<_>, _>>()?;
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
        private(root)?;
        let (kept, full) = put_back(root)?;
        let _ = std::fs::remove_file(&state);
        let mut others = match kept {
            0 => String::new(),
            n => format!(" with the {n} facts other agents saved or removed meanwhile"),
        };
        for dir in full {
            others += &format!(
                " ({} is now past {LIMIT} bytes of index: merge facts there first)",
                dir.display()
            );
        }
        return Err(format!(
            "loss_guard: the cleanup removed {} of {before} facts, above {limit}; memory is back as it was at {start}{others}, and the refused cleanup is commit {head}",
            removed.len()
        ));
    }
    let _ = std::fs::remove_file(root.join(OTHERS));
    // A refused cleanup saved nothing of what the person wrote, so the
    // next one reads it again.
    if let Some(at_ms) = open["at_ms"].as_i64() {
        write(&root.join(SAID), &at_ms.to_string())?;
    }
    std::fs::remove_file(&state).map_err(|e| format!("memory_failed: {}: {e}", state.display()))?;
    Ok(
        json!({"commit": head, "facts": after, "removed": removed, "added": added, "changed": changed}),
    )
}

/// The trigger that runs the cleanup each night: an agent of its own,
/// started on the first fire in the memory folder, woken only when
/// `changed` says so, and each turn capped.
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
    let quote = format!("'{}'", script.to_string_lossy().replace('\'', r"'\''"));
    let mut add = vec![
        "--name".into(),
        "memory-cleanup".into(),
        "--cron".into(),
        cron.unwrap_or_else(|| "30 3 * * *".into()),
    ];
    // A first schedule starts the agent; scheduling again, say at another
    // time, keeps the one it started, its model and its history.
    match (model, effort) {
        (Some(model), effort) => {
            add.extend(["--start".into(), AGENT.into(), "--model".into(), model]);
            if let Some(effort) = effort {
                add.extend(["--effort".into(), effort]);
            }
        }
        (None, None) => add.extend(["--bot".into(), AGENT.into()]),
        (None, Some(_)) => return Err(usage()),
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

const AGENT: &str = "memory-cleanup";

fn schedule(home: &Path, args: &[String]) -> Result<Value, String> {
    let add = schedule_args(&home.join(".agent/memory"), args)?;
    // The agent the trigger starts works in the memory folder.
    let root = home.join(".agents/memory");
    repo(&root)?;
    std::env::set_current_dir(&root)
        .map_err(|e| format!("memory_failed: {}: {e}", root.display()))?;
    add_trigger(&home.join(".agent/trigger"), &add).map_err(|error| match error.split_once(':') {
        Some(("bot_exists", _)) => format!(
            "bot_exists: {AGENT} is an agent already: schedule without --model to keep it, or remove it with agent rm --bot {AGENT} to start it on another model"
        ),
        Some(("bot_not_found", _)) if !add.iter().any(|a| a == "--start") => format!(
            "bot_not_found: there is no {AGENT} agent yet: schedule with --model to start one"
        ),
        _ => error,
    })
}

/// `~/.agent/trigger add ARGS`, run in this folder with this shell's
/// daemon and agent: its JSON line, or its error as `CODE: detail`.
fn add_trigger(script: &Path, args: &[String]) -> Result<Value, String> {
    if !script.is_file() {
        return Err(format!(
            "triggers_unsupported: {} is not there to add the nightly cleanup",
            script.display()
        ));
    }
    let out = std::process::Command::new(script)
        .arg("add")
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("memory_failed: {}: {e}", script.display()))?;
    if out.status.success() {
        return serde_json::from_slice(&out.stdout)
            .map_err(|e| format!("memory_failed: {}: {e}", script.display()));
    }
    let error: Value = serde_json::from_slice(&out.stderr).unwrap_or_default();
    Err(match (error["error"].as_str(), error["detail"].as_str()) {
        (Some(code), Some(detail)) => format!("{code}: {detail}"),
        _ => format!(
            "trigger_failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    })
}

/// Project folders the Memory sheet at Home lists at most; the rest are
/// counted.
const MAX_PROJECTS: usize = 100;

/// What the app shows of memory, and what it tells a coordinator changed:
/// the person's facts, and `project`'s, or with no project every project's,
/// each fact with its file and when it was last written. A folder that is not
/// all facts says so in place of its facts.
pub fn view(root: &Path, project: Option<&str>) -> Value {
    let scope = |dir: PathBuf, name: Option<&str>| {
        let listed = facts(&dir).map(|all| {
            all.iter()
                .map(|fact| {
                    let path = dir.join(format!("{}.md", fact.name));
                    let modified = std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map_or(0, |d| d.as_millis() as u64);
                    json!({"name": fact.name, "type": fact.kind, "description": fact.description,
                        "source": fact.source, "verified": fact.verified, "path": path, "modified": modified})
                })
                .collect::<Vec<_>>()
        });
        let mut out = json!({"name": name, "dir": dir});
        match listed {
            Ok(facts) => out["facts"] = json!(facts),
            Err(error) => out["error"] = json!(error),
        }
        out
    };
    let projects = root.join("projects");
    let (names, more) = match project {
        Some(name) => (vec![name.to_owned()], 0),
        None => {
            let mut names: Vec<String> = std::fs::read_dir(&projects)
                .into_iter()
                .flatten()
                .take(MAX_ENTRIES)
                .flatten()
                .filter(|e| e.path().is_dir())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|name| crate::project::valid_name(name))
                .collect();
            names.sort();
            let more = names.len().saturating_sub(MAX_PROJECTS);
            names.truncate(MAX_PROJECTS);
            (names, more)
        }
    };
    let listed: Vec<Value> = names
        .iter()
        .map(|name| scope(projects.join(name), Some(name)))
        .collect();
    json!({"user": scope(root.to_path_buf(), None), "projects": listed, "more": more})
}

/// What `~/.agent/memory` says it takes.
pub fn usage() -> &'static str {
    USAGE
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

    #[test]
    fn view_lists_the_persons_facts_and_a_projects() {
        let root = temp("view");
        save_user(&root, "short-replies", "keep replies short", "Short.").unwrap();
        let shown = view(&root, None);
        let fact = &shown["user"]["facts"][0];
        assert_eq!(fact["name"], "short-replies");
        assert_eq!(fact["description"], "keep replies short");
        assert_eq!(fact["path"], json!(root.join("short-replies.md")));
        assert!(fact["modified"].as_u64().unwrap() > 0);
        assert_eq!(shown["projects"], json!([]));
        // Every project's at Home; one project's in it.
        let project = root.join("projects/demo");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("bad.md"), "not a fact").unwrap();
        let shown = view(&root, None);
        assert_eq!(shown["projects"][0]["name"], "demo");
        assert!(
            shown["projects"][0]["error"]
                .as_str()
                .unwrap()
                .starts_with("memory_invalid: ")
        );
        assert_eq!(view(&root, Some("demo"))["projects"][0]["name"], "demo");
        assert_eq!(view(&root, Some("new"))["projects"][0]["facts"], json!([]));
        let _ = std::fs::remove_dir_all(&root);
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
    fn an_unscoped_read_finds_the_persons_fact_from_a_project_and_from_outside_one() {
        let home = temp("read-scopes");
        let root = home.join(".agents/memory");
        let work = home.join("work");
        std::fs::create_dir_all(work.join(".agents")).unwrap();
        std::fs::write(work.join(".agents/project.toml"), "name = \"demo\"\n").unwrap();
        let outside = home.join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        save_user(&root, "short-replies", "keep replies short", "Short.").unwrap();
        let read = |cwd: &Path, line: &[&str]| run(&root, cwd, &args(line), &no_stdin);
        let found = read(&work, &["read", "short-replies"]).unwrap();
        assert!(found["text"].as_str().unwrap().contains("Short."));
        let found = read(&outside, &["read", "short-replies"]).unwrap();
        assert!(found["text"].as_str().unwrap().contains("Short."));
        // A project's own fact of the same name comes first.
        read(
            &work,
            &[
                "save",
                "short-replies",
                "--type",
                "feedback",
                "--description",
                "d",
                "--source",
                "s",
                "--",
                "Project.",
            ],
        )
        .unwrap();
        let found = read(&work, &["read", "short-replies"]).unwrap();
        assert!(found["text"].as_str().unwrap().contains("Project."));
        // A scope given is the only one read.
        let error = read(&work, &["read", "short-replies", "--project", "other"]).unwrap_err();
        assert!(error.starts_with("fact_not_found: "), "{error}");
        let error = read(&work, &["read", "missing"]).unwrap_err();
        assert!(error.starts_with("fact_not_found: missing: "), "{error}");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_read_is_recorded_beside_the_facts_and_the_cleanup_hears_of_old_unread_ones() {
        let home = temp("reads");
        let root = home.join(".agents/memory");
        let code = |r: Result<Value, String>| error_json(&r.unwrap_err())["error"].clone();
        let run_ = |line: &[&str]| run(&root, &root, &args(line), &no_stdin);
        save_user(&root, "a", "fact a", "the text of a").unwrap();
        let index = std::fs::read_to_string(root.join(INDEX)).unwrap();
        let first = run_(&["read", "a", "--user"]).unwrap();
        assert_eq!(
            (first["read"].clone(), first["reads"].clone()),
            (json!("a"), json!(1))
        );
        assert!(first["text"].as_str().unwrap().contains("the text of a"));
        assert!(
            first["text"]
                .as_str()
                .unwrap()
                .starts_with("---\nname: a\n")
        );
        // The tool's call is the script's.
        let second = call(&home, &home, &json!({"read": "a", "user": true})).unwrap();
        assert_eq!(second["reads"], 2);
        // A read changes no fact and no index, and its record is no fact.
        assert_eq!(std::fs::read_to_string(root.join(INDEX)).unwrap(), index);
        assert_eq!(check(&root)["problems"], json!([]));
        assert_eq!(reads(&root)["a"], json!({"last": today(), "count": 2}));
        assert_eq!(code(run_(&["read", "zz", "--user"])), "fact_not_found");
        // Reading a fact of no project makes no folder for it.
        assert_eq!(
            code(run_(&["read", "zz", "--project", "none"])),
            "fact_not_found"
        );
        assert!(!root.join("projects/none").exists());
        assert_eq!(code(run_(&["read", "--user"])), "usage");
        // Facts nobody read or saved for 30 days are listed, oldest first;
        // a read takes a fact off the list.
        let old = |name: &str, verified: &str| {
            let fact = Fact {
                name: name.into(),
                kind: "project".into(),
                description: format!("old fact {name}"),
                source: "the person, 2020-01-01".into(),
                verified: verified.into(),
                external: false,
                confirmed: 0,
                pushed_back: 0,
            };
            std::fs::write(root.join(format!("{name}.md")), fact.render("old")).unwrap();
        };
        old("c", "2020-01-01");
        old("d", "2020-01-01");
        old("e", "2020-03-01");
        let mut record = reads(&root);
        record.insert("d".into(), json!({"last": "2020-02-01", "count": 1}));
        write(&root.join(READS), &Value::Object(record).to_string()).unwrap();
        assert_eq!(
            unread(&root).unwrap(),
            json!([
                {"path": "c.md", "last_read": null, "verified": "2020-01-01"},
                {"path": "d.md", "last_read": "2020-02-01", "verified": "2020-01-01"},
                {"path": "e.md", "last_read": null, "verified": "2020-03-01"}
            ])
            .as_array()
            .unwrap()
            .clone()
        );
        run_(&["index", "--user"]).unwrap();
        run_(&["read", "c", "--user"]).unwrap();
        let started = run_(&["cleanup", "start"]).unwrap();
        assert_eq!(
            started["unread"],
            json!([
                {"path": "d.md", "last_read": "2020-02-01", "verified": "2020-01-01"},
                {"path": "e.md", "last_read": null, "verified": "2020-03-01"}
            ])
        );
        // Removing a fact forgets its reads.
        run_(&["rm", "d", "--user"]).unwrap();
        assert!(reads(&root).get("d").is_none());
        run_(&["cleanup", "finish"]).unwrap();
        // Reads are not changes to clean up, also where the ignore file
        // was written before they were recorded.
        run_(&["read", "a", "--user"]).unwrap();
        assert_eq!(code(changed(&root)), "unchanged");
        write(&root.join(".gitignore"), ".lock\n/.cleanup\n.DS_Store\n").unwrap();
        git(&root, &["commit", "-q", "-am", "older ignore file"]).unwrap();
        run_(&["read", "a", "--user"]).unwrap();
        assert_eq!(code(changed(&root)), "unchanged");
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_fact_saved_after_outside_content_says_so_in_its_file_and_the_index() {
        let home = temp("outside");
        let root = home.join(".agents/memory");
        let code = |r: Result<Value, String>| error_json(&r.unwrap_err())["error"].clone();
        let save = json!({"save": {"name": "pinned-version", "type": "reference",
            "description": "The SDK pin a release note named", "source": "turn:shop.lead/4",
            "text": "Pin the SDK at 2.3.", "external": true}, "user": true});
        call(&home, &home, &save).unwrap();
        let file = std::fs::read_to_string(root.join("pinned-version.md")).unwrap();
        assert!(file.contains("\nverified: ") && file.contains("\nexternal: true\n---\n"));
        let index = std::fs::read_to_string(root.join(INDEX)).unwrap();
        assert!(
            index.contains(", external): The SDK pin a release note named"),
            "{index}"
        );
        assert_eq!(check(&root)["problems"], json!([]));
        // Saved again without it, from the person's word, it is theirs.
        let mut plain = save.clone();
        plain["save"]["external"] = json!(false);
        plain["save"]["source"] = json!("the person, 2026-10-29");
        call(&home, &home, &plain).unwrap();
        let file = std::fs::read_to_string(root.join("pinned-version.md")).unwrap();
        assert!(!file.contains("external"));
        assert!(
            !std::fs::read_to_string(root.join(INDEX))
                .unwrap()
                .contains("external")
        );
        // The script takes it as a flag, once.
        let run_ = |line: &[&str]| run(&root, &home, &args(line), &no_stdin);
        let line = |extra: &[&str]| {
            let mut line = vec![
                "save",
                "x",
                "--type",
                "project",
                "--description",
                "d",
                "--source",
                "s",
                "--user",
            ];
            line.extend_from_slice(extra);
            line.extend_from_slice(&["--", "t"]);
            line.iter().map(|w| w.to_string()).collect::<Vec<_>>()
        };
        fn refs(words: &[String]) -> Vec<&str> {
            words.iter().map(String::as_str).collect()
        }
        run_(&refs(&line(&["--external"]))).unwrap();
        assert!(
            std::fs::read_to_string(root.join("x.md"))
                .unwrap()
                .contains("external: true")
        );
        assert_eq!(
            code(run_(&refs(&line(&["--external", "--external"])))),
            "usage"
        );
        assert_eq!(code(run_(&["rm", "x", "--external", "--user"])), "usage");
        // Any other value is not a fact.
        std::fs::write(
            root.join("y.md"),
            "---\nname: y\ndescription: d\ntype: project\nsource: s\nverified: 2026-10-29\nexternal: yes\n---\n\nt\n",
        )
        .unwrap();
        assert_eq!(code(run_(&["index", "--user"])), "memory_invalid");
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_facts_counts_show_in_the_index_and_outlast_a_save_without_them() {
        let home = temp("counts");
        let root = home.join(".agents/memory");
        let code = |r: Result<Value, String>| error_json(&r.unwrap_err())["error"].clone();
        let save = json!({"save": {"name": "split-reviews", "type": "feedback",
            "description": "Reviews go to a task of their own", "source": "the person, 2026-10-30",
            "text": "Start a task for each review.", "confirmed": 3, "pushed_back": 1}, "user": true});
        call(&home, &home, &save).unwrap();
        let file = || std::fs::read_to_string(root.join("split-reviews.md")).unwrap();
        assert!(
            file().contains("\nconfirmed: 3\npushed_back: 1\n---\n"),
            "{}",
            file()
        );
        let index = || std::fs::read_to_string(root.join(INDEX)).unwrap();
        assert!(
            index().contains(", confirmed 3, pushed back 1): Reviews go to a task of their own"),
            "{}",
            index()
        );
        // Reworded without counts, the fact keeps its weight.
        let mut reworded = save.clone();
        reworded["save"]
            .as_object_mut()
            .unwrap()
            .remove("confirmed");
        reworded["save"]
            .as_object_mut()
            .unwrap()
            .remove("pushed_back");
        reworded["save"]["text"] = json!("Each review is a task.");
        call(&home, &home, &reworded).unwrap();
        assert!(file().contains("confirmed: 3") && file().contains("pushed_back: 1"));
        // A count given is the count; zero leaves it out of file and index.
        reworded["save"]["confirmed"] = json!(4);
        reworded["save"]["pushed_back"] = json!(0);
        call(&home, &home, &reworded).unwrap();
        assert!(file().contains("confirmed: 4") && !file().contains("pushed_back"));
        assert!(index().contains(", confirmed 4): "), "{}", index());
        // The script takes them as flags, once each, as whole numbers.
        let run_ = |line: &[&str]| run(&root, &home, &args(line), &no_stdin);
        let base = [
            "save",
            "x",
            "--type",
            "project",
            "--description",
            "d",
            "--source",
            "s",
            "--user",
        ];
        let line = |extra: &[&str]| {
            let mut line: Vec<&str> = base.to_vec();
            line.extend_from_slice(extra);
            line.extend_from_slice(&["--", "t"]);
            run_(&line)
        };
        line(&["--pushed-back", "2"]).unwrap();
        assert!(
            std::fs::read_to_string(root.join("x.md"))
                .unwrap()
                .contains("pushed_back: 2")
        );
        assert_eq!(
            code(line(&["--confirmed", "1", "--confirmed", "2"])),
            "usage"
        );
        assert_eq!(code(line(&["--confirmed", "many"])), "usage");
        assert_eq!(
            code(run_(&["rm", "x", "--confirmed", "1", "--user"])),
            "usage"
        );
        assert_eq!(check(&root)["problems"], json!([]));
        // Any other value is not a fact.
        std::fs::write(
            root.join("y.md"),
            "---\nname: y\ndescription: d\ntype: project\nsource: s\nverified: 2026-10-29\nconfirmed: lots\n---\n\nt\n",
        )
        .unwrap();
        assert_eq!(code(run_(&["index", "--user"])), "memory_invalid");
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn dates_count_days_from_1970() {
        assert_eq!(day_number("1970-01-01"), Some(0));
        assert_eq!(day_number("2000-03-01"), Some(11_017));
        assert_eq!(
            day_number("2026-10-29").unwrap() - day_number("2026-09-29").unwrap(),
            30
        );
        assert_eq!(day_number("2026-13-01"), None);
        assert_eq!(day_number("today"), None);
    }

    #[test]
    fn a_cleanup_is_committed_or_reverted_by_the_loss_guard() {
        let root = temp("cleanup");
        let code = |r: Result<Value, String>| error_json(&r.unwrap_err())["error"].clone();
        let run_ = |line: &[&str]| run(&root, &root, &args(line), &no_stdin);
        assert_eq!(code(changed(&root)), "unchanged");
        // A new repository is no change to clean up.
        repo(&root).unwrap();
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
        assert_eq!(started["since_ms"], Value::Null);
        // A start while one is open is that one, so the guard counts from it.
        let again = run_(&["cleanup", "start"]).unwrap();
        assert_eq!(
            (again["started"].clone(), again["duplicate"].clone()),
            (started["started"].clone(), json!(true))
        );
        assert_eq!(again["saved"], started["saved"]);
        // An open cleanup counts as a change, so one cut short is finished
        // the next night.
        assert_eq!(changed(&root).unwrap(), json!({"changed": true}));
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
        // The next one reads what the person wrote from when the last
        // finished one started.
        let since = next["since_ms"].as_i64().unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        assert!(now - since < 60_000, "{since}");
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
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        for name in ["a", "c", "d", "e"] {
            // Put back as private as `save` wrote it.
            assert_eq!(mode(&root.join(format!("{name}.md"))), 0o600, "{name}");
        }
        // No object of the repository is readable by anyone else.
        let objects = root.join(".git/objects");
        for dir in std::fs::read_dir(&objects).unwrap().flatten() {
            assert_eq!(mode(&dir.path()) & 0o077, 0, "{}", dir.path().display());
        }
        assert!(
            std::fs::read_to_string(root.join(INDEX))
                .unwrap()
                .contains("- d (feedback")
        );
        assert_eq!(check(&root)["problems"], json!([]));
        assert_eq!(code(changed(&root)), "unchanged");
        assert!(!root.join(CLEANUP).exists());
        // A refused cleanup leaves what the person wrote to the next one.
        let after = run_(&["cleanup", "start"]).unwrap();
        assert_eq!(after["since_ms"], json!(since));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_refused_cleanup_keeps_what_other_agents_saved_meanwhile() {
        let root = temp("cleanup-others");
        let as_ = |caller: Option<&str>, line: &[&str]| {
            run_as(&root, &root, &args(line), &|| Ok("-".into()), caller)
        };
        let save = |caller, name: &str, text: &str| {
            let mut line = vec!["save", name, "--type", "feedback", "--description"];
            line.extend([name, "--source", "test", "--user", "--", text]);
            as_(caller, &line).unwrap();
        };
        repo(&root).unwrap();
        for name in ["a", "b", "c", "d"] {
            save(None, name, "before");
        }
        // The cleanup's agent is 7; agent 8 and the person save beside it.
        as_(Some("7"), &["cleanup", "start"]).unwrap();
        save(Some("8"), "c", "changed by 8");
        save(Some("8"), "e", "new from 8");
        save(None, "f", "new from the person");
        save(Some("8"), "g", "saved then removed by 8");
        as_(Some("8"), &["rm", "g", "--user"]).unwrap();
        save(Some("7"), "a", "merged by the cleanup");
        for name in ["b", "c", "d", "e"] {
            as_(Some("7"), &["rm", name, "--user"]).unwrap();
        }
        let refused = as_(Some("7"), &["cleanup", "finish"]).unwrap_err();
        assert!(
            refused.contains("with the 4 facts other agents saved or removed meanwhile"),
            "{refused}"
        );
        let text = |name: &str| std::fs::read_to_string(root.join(format!("{name}.md"))).ok();
        // The cleanup's own changes are taken back; the others' stay, each
        // as its agent last left it, even one the cleanup then removed.
        assert!(text("a").unwrap().ends_with("before\n"), "{:?}", text("a"));
        assert!(text("b").unwrap().ends_with("before\n"));
        assert!(text("c").unwrap().ends_with("changed by 8\n"));
        assert!(text("e").unwrap().ends_with("new from 8\n"));
        assert!(text("f").unwrap().ends_with("new from the person\n"));
        assert_eq!(text("g"), None);
        assert_eq!(check(&root)["problems"], json!([]));
        assert!(
            std::fs::read_to_string(root.join(INDEX))
                .unwrap()
                .contains("- e (")
        );
        // Committed, and the next cleanup starts clean.
        assert_eq!(
            error_json(&changed(&root).unwrap_err())["error"],
            "unchanged"
        );
        assert!(!root.join(OTHERS).exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_refused_cleanup_says_when_what_it_kept_overfills_the_index() {
        let root = temp("cleanup-full");
        let long = "d".repeat(3500);
        let save = |caller: Option<&str>, name: &str| {
            let line = [
                "save",
                name,
                "--type",
                "feedback",
                "--description",
                &long,
                "--source",
                "test",
                "--user",
                "--",
                "text",
            ];
            run_as(&root, &root, &args(&line), &|| Ok("-".into()), caller).unwrap();
        };
        repo(&root).unwrap();
        for name in ["a", "b", "c", "d"] {
            save(None, name);
        }
        let as_7 = |line: &[&str]| run_as(&root, &root, &args(line), &|| Ok("-".into()), Some("7"));
        as_7(&["cleanup", "start"]).unwrap();
        for name in ["b", "c", "d"] {
            as_7(&["rm", name, "--user"]).unwrap();
        }
        // Each save fits in the room the cleanup made.
        for name in ["e", "f", "g"] {
            save(Some("8"), name);
        }
        let refused = as_7(&["cleanup", "finish"]).unwrap_err();
        assert!(refused.contains("with the 3 facts"), "{refused}");
        assert!(refused.contains("merge facts there first"), "{refused}");
        // Nothing saved is dropped to fit.
        for name in ["a", "b", "c", "d", "e", "f", "g"] {
            assert!(root.join(format!("{name}.md")).exists(), "{name}");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_schedule_is_a_gated_capped_trigger_that_starts_or_keeps_its_own_agent() {
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
        // Without a model, the agent it started before.
        let again = line(&["--cron", "0 5 * * *"]).unwrap();
        assert_eq!(again[3..6], args(&["0 5 * * *", "--bot", "memory-cleanup"]));
        assert_eq!(line(&[]).unwrap()[4..6], args(&["--bot", "memory-cleanup"]));
        for bad in [
            &["--effort", "low"][..],
            &["--model"],
            &["--model", "m", "--model", "n"],
            &["--every", "1d"],
        ] {
            assert!(line(bad).unwrap_err().starts_with("usage: "), "{bad:?}");
        }
    }
}
