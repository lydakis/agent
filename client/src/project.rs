//! A project is a folder, its coordinator bot `NAME.lead`, and
//! `.agents/project.toml` in that folder, which holds mechanics only: the
//! name, the coordinator's model and effort level, and its tasks' model and
//! effort and whether they work in their own worktrees or the project
//! folder. Clients read it, and write it when they make a project; the
//! daemon knows nothing of projects.
use serde_json::{Value, json};
use std::io::Read;
use std::path::Path;

pub const FILE: &str = ".agents/project.toml";
/// The largest project file the reader takes.
pub const LIMIT: u64 = 64 * 1024;
/// Every key the file may hold; anything else is a mistake, not ignored.
const KEYS: [&str; 7] = [
    "name",
    "coordinator",
    "model",
    "reasoning",
    "threads_model",
    "threads_reasoning",
    "threads_in",
];
/// Where a project's threads work: each in its own worktree, or all in
/// the project folder.
pub const THREADS_IN: [&str; 2] = ["worktree", "project"];
/// How a task that works apart from the project folder gets its own copy
/// of it: a git worktree, or a sandfs copy (of the checkout as it is,
/// uncommitted changes included, or of a folder outside git). The client's
/// preference, not the project's, so any folder can use either.
pub const COPIES: [&str; 2] = ["worktree", "sandfs"];

/// A name that is also a bot-name prefix: the daemon's name characters,
/// short enough that `NAME.lead` and its task names fit.
pub fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        && !name.starts_with('.')
        && !name.ends_with('.')
}

/// The folder's own name, reduced to name characters.
fn default_name(dir: &Path) -> String {
    let base = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let name: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let name = name.trim_matches('-');
    let name = &name[..name.len().min(64)];
    if name.is_empty() {
        "project".into()
    } else {
        name.into()
    }
}

/// The project in `dir` (a canonical directory): the file's fields when it
/// exists, or the defaults a new project there would take. `file` says which.
pub fn read(dir: &Path) -> Result<Value, String> {
    let path = dir.join(FILE);
    let invalid = |reason: &str| format!("project_invalid: {}: {reason}", path.display());
    let text = match std::fs::File::open(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("project_unreadable: {}: {error}", path.display())),
        Ok(file) => {
            if !file
                .metadata()
                .map_err(|e| invalid(&e.to_string()))?
                .is_file()
            {
                return Err(invalid("not a regular file"));
            }
            let mut text = String::new();
            file.take(LIMIT + 1)
                .read_to_string(&mut text)
                .map_err(|e| format!("project_unreadable: {}: {e}", path.display()))?;
            if text.len() as u64 > LIMIT {
                return Err(invalid("larger than 64 KiB"));
            }
            Some(text)
        }
    };
    let Some(text) = text else {
        let name = default_name(dir);
        return Ok(json!({
            "dir": dir, "name": name, "coordinator": format!("{name}.lead"),
            "model": null, "reasoning": null, "threads_model": null,
            "threads_reasoning": null, "threads_in": "worktree", "file": false,
        }));
    };
    let table: toml::Table = text
        .parse()
        .map_err(|e: toml::de::Error| invalid(e.message()))?;
    if let Some(key) = table.keys().find(|key| !KEYS.contains(&key.as_str())) {
        return Err(invalid(&format!("unknown key {key}")));
    }
    let field = |key: &str| match table.get(key) {
        None => Ok(None),
        Some(toml::Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(invalid(&format!("{key} must be a string"))),
    };
    let name = field("name")?.ok_or_else(|| invalid("name is required"))?;
    if !valid_name(&name) {
        return Err(invalid("name must be 1-64 of A-Z a-z 0-9 - _ ."));
    }
    let coordinator = format!("{name}.lead");
    if field("coordinator")?.is_some_and(|c| c != coordinator) {
        return Err(invalid(&format!("coordinator must be {coordinator}")));
    }
    let model = field("model")?;
    if model.as_deref() == Some("") {
        return Err(invalid("model must not be empty"));
    }
    // The daemon judges models and levels when the agents are made.
    let said = |key: &str| {
        let value = field(key)?;
        if value.as_deref() == Some("") {
            return Err(invalid(&format!("{key} must not be empty")));
        }
        Ok(value)
    };
    let (reasoning, threads_model, threads_reasoning) = (
        said("reasoning")?,
        said("threads_model")?,
        said("threads_reasoning")?,
    );
    if threads_reasoning.is_some() && threads_model.is_none() {
        return Err(invalid("threads_reasoning goes with threads_model"));
    }
    let threads_in = field("threads_in")?.unwrap_or_else(|| "worktree".into());
    if !THREADS_IN.contains(&threads_in.as_str()) {
        return Err(invalid("threads_in must be \"worktree\" or \"project\""));
    }
    Ok(json!({
        "dir": dir, "name": name, "coordinator": coordinator, "model": model,
        "reasoning": reasoning, "threads_model": threads_model,
        "threads_reasoning": threads_reasoning, "threads_in": threads_in, "file": true,
    }))
}

/// A project's task settings, said to its coordinator as the flags its
/// starts take. The model is always named, so a role a task starts in
/// (--profile) cannot swap it: the one picked, else the lead's own, which
/// its shell holds as AGENT_MODEL (and its effort as AGENT_EFFORT). A
/// picked model goes in quoted, since a model id may hold characters a
/// shell would act on. `copies` is one of `COPIES`.
pub fn tasks_rule(
    model: Option<&str>,
    effort: Option<&str>,
    in_project: bool,
    copies: &str,
) -> String {
    let quote = |v: &str| format!("'{}'", v.replace('\'', r"'\''"));
    let flags = match model {
        Some(model) => format!(
            "--model {}{}",
            quote(model),
            effort.map_or(String::new(), |e| format!(" --effort {}", quote(e)))
        ),
        None => r#"--model "$AGENT_MODEL" ${AGENT_EFFORT:+--effort "$AGENT_EFFORT"}"#.into(),
    };
    let place = match (in_project, copies) {
        (true, _) => "Every task works in this folder, with no worktree of its own.",
        (false, "sandfs") => {
            "Every task works in its own sandfs copy, git repository or not, so tasks do not collide: only you work in this folder."
        }
        (false, _) => {
            "When this folder is a git repository, every task works in its own worktree, so tasks do not collide: only you work in this folder."
        }
    };
    format!(
        "This project's tasks, as the person set them up: start every new task, in a role (--profile) or not, with {flags}. {place}"
    )
}

/// The task rule a project's file sets, with the client's `copies`, as
/// its coordinator is told it.
pub fn rule(info: &Value, copies: &str) -> String {
    tasks_rule(
        info["threads_model"].as_str(),
        info["threads_reasoning"].as_str(),
        info["threads_in"] == "project",
        copies,
    )
}

/// What a new project's threads run on and where they work.
#[derive(Debug, Default)]
pub struct Threads<'a> {
    pub model: Option<&'a str>,
    pub reasoning: Option<&'a str>,
    pub in_project: bool,
}

/// Write a new project's file. An existing file is the user's and is kept:
/// one that appeared since the folder was read is refused, to be read again.
pub fn write(
    dir: &Path,
    name: &str,
    model: &str,
    reasoning: Option<&str>,
    threads: &Threads,
) -> Result<(), String> {
    if !valid_name(name) {
        return Err("project_invalid: name must be 1-64 of A-Z a-z 0-9 - _ .".into());
    }
    let path = dir.join(FILE);
    let quote = |s: &str| toml::Value::String(s.to_owned()).to_string();
    let mut text = format!(
        "name = {}\ncoordinator = {}\nmodel = {}\n",
        quote(name),
        quote(&format!("{name}.lead")),
        quote(model)
    );
    fn some(value: Option<&str>) -> Option<&str> {
        value.filter(|v| !v.is_empty())
    }
    if let Some(level) = some(reasoning) {
        text.push_str(&format!("reasoning = {}\n", quote(level)));
    }
    if let Some(model) = some(threads.model) {
        text.push_str(&format!("threads_model = {}\n", quote(model)));
        if let Some(level) = some(threads.reasoning) {
            text.push_str(&format!("threads_reasoning = {}\n", quote(level)));
        }
    }
    text.push_str(&format!(
        "threads_in = {}\n",
        quote(THREADS_IN[usize::from(threads.in_project)])
    ));
    let failed = |e: std::io::Error| match e.kind() {
        std::io::ErrorKind::AlreadyExists => {
            format!(
                "project_changed: {} appeared; open the folder again",
                path.display()
            )
        }
        _ => format!("project_unwritable: {}: {e}", path.display()),
    };
    let agent = dir.join(".agents");
    let created = !agent.is_dir();
    std::fs::create_dir_all(&agent).map_err(failed)?;
    place_new(&path, |file| {
        std::io::Write::write_all(file, text.as_bytes())?;
        file.sync_all()
    })
    .map_err(failed)?;
    // The new entries are durable only once their directories are synced.
    let sync = |d: &Path| std::fs::File::open(d).and_then(|f| f.sync_all());
    sync(&agent).map_err(failed)?;
    if created {
        sync(dir).map_err(failed)?;
    }
    Ok(())
}

/// Fill a temporary file beside `path`, then link it into place. The link
/// refuses an existing file, which is kept, and `path` never holds a partial
/// file: a failed fill leaves nothing behind.
fn place_new(
    path: &Path,
    fill: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let temp = path.with_extension(format!("toml.{}.{nanos}.tmp", std::process::id()));
    let result = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .and_then(|mut file| fill(&mut file))
        .and_then(|()| std::fs::hard_link(&temp, path));
    let removed = std::fs::remove_file(&temp);
    result?;
    match removed {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// What a project's creator asked for: the coordinator's model and effort,
/// and its threads' picks (none: the coordinator picks their model, and
/// they get worktrees). A folder whose file names a model keeps the file's
/// settings instead, and the picks are not used.
#[derive(Debug, Default)]
pub struct Picks {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub threads: Option<ThreadPicks>,
}

#[derive(Debug, Default)]
pub struct ThreadPicks {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub in_project: bool,
}

/// A daemon that answers requests: the app's client, or a test's.
pub trait Requests {
    fn request(
        &self,
        op: &str,
        params: Value,
    ) -> impl std::future::Future<Output = crate::Result<Value>> + Send;
}

impl Requests for crate::Client {
    async fn request(&self, op: &str, params: Value) -> crate::Result<Value> {
        crate::Client::request(self, op, params).await
    }
}

/// Make the project in `dir` (a canonical directory): its coordinator in
/// its role (`policy`, as composed for the folder), told its tasks'
/// settings with the client's `copies` and then `after`, the client's own
/// word, then the folder's file when it had none, once the daemon took
/// the coordinator. A project that already exists in this folder is
/// returned with `created: false`, its coordinator's row as the record, and
/// nothing changed; a name another
/// folder's coordinator holds is refused.
pub async fn create(
    daemon: &impl Requests,
    dir: &Path,
    policy: &Value,
    tools: &Value,
    picks: &Picks,
    copies: &str,
    after: Option<&str>,
) -> Result<Value, String> {
    let info = read(dir)?;
    let (name, coordinator) = (
        info["name"].as_str().unwrap_or_default(),
        info["coordinator"].as_str().unwrap_or_default(),
    );
    let at = dir
        .to_str()
        .ok_or("project_invalid: the folder must be valid UTF-8")?;
    let made = |created: bool, record: Value| {
        json!({"project": name, "coordinator": coordinator, "dir": at, "created": created,
               "from_file": info["file"] == true, "record": record})
    };
    let existing = async || -> Result<Option<Value>, String> {
        let page = daemon
            .request("bots", json!({"name": coordinator, "limit": 1}))
            .await
            .map_err(coded)?;
        let Some(bot) = page["bots"].get(0).filter(|b| b["name"] == coordinator) else {
            return Ok(None);
        };
        match bot["workspace"].as_str() {
            Some(have) if have == at => Ok(Some(made(false, bot.clone()))),
            have => Err(format!(
                "project_exists: {coordinator} already belongs to {}",
                have.unwrap_or("another folder")
            )),
        }
    };
    if let Some(found) = existing().await? {
        return Ok(found);
    }
    // A folder whose file names a model keeps its settings; a new one takes
    // the picks, else its role's model.
    let file = info["file"] == true;
    let kept = file && info["model"].is_string();
    let model = if kept {
        info["model"].as_str()
    } else {
        picks.model.as_deref().or(policy["model"].as_str())
    }
    .ok_or("model_required: choose a model")?;
    let effort = if kept {
        info["reasoning"].as_str()
    } else {
        picks.effort.as_deref()
    };
    let rule = if file {
        rule(&info, copies)
    } else {
        let t = picks.threads.as_ref();
        tasks_rule(
            t.and_then(|t| t.model.as_deref()),
            t.and_then(|t| t.effort.as_deref()),
            t.is_some_and(|t| t.in_project),
            copies,
        )
    };
    let mut instructions = format!(
        "{}\n\n{rule}",
        policy["instructions"].as_str().unwrap_or_default()
    );
    if let Some(after) = after {
        instructions.push_str("\n\n");
        instructions.push_str(after);
    }
    let mut params = json!({
        "bot": coordinator, "workspace": at, "model": model,
        "instructions": instructions,
        "compaction_instructions": policy["compaction_instructions"],
        "tools": if policy["tools"].is_null() { tools } else { &policy["tools"] },
    });
    if let Some(effort) = effort {
        params["effort"] = json!(effort);
    }
    if let Some(ttl) = crate::policy::cache_ttl(model) {
        params["settings"] = json!({"cache_ttl": ttl});
    }
    // The tools its client declares on the lead, from its first turn.
    if !policy["declare"].is_null() {
        params["declare"] = policy["declare"].clone();
    }
    let record = match daemon.request("create", params).await {
        Ok(record) => record,
        // Made since the look above: as when it was there before.
        Err(error) if error.code == "bot_exists" => {
            return existing().await?.ok_or_else(|| coded(error));
        }
        Err(error) => return Err(coded(error)),
    };
    if !file {
        let t = picks.threads.as_ref();
        write(
            dir,
            name,
            model,
            effort,
            &Threads {
                model: t.and_then(|t| t.model.as_deref()),
                reasoning: t.and_then(|t| t.effort.as_deref()),
                in_project: t.is_some_and(|t| t.in_project),
            },
        )?;
    }
    Ok(made(true, record))
}

/// A daemon's error as `CODE: detail`, its code kept for software.
fn coded(error: crate::Error) -> String {
    format!("{}: {}", error.code, error.detail.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rule_says_how_tasks_get_their_copies() {
        let info =
            json!({"threads_model": null, "threads_reasoning": null, "threads_in": "worktree"});
        assert!(
            rule(&info, "worktree")
                .ends_with("every task works in its own worktree, so tasks do not collide: only you work in this folder.")
        );
        assert!(rule(&info, "sandfs").ends_with(
            "Every task works in its own sandfs copy, git repository or not, so tasks do not collide: only you work in this folder."
        ));
        let shared =
            json!({"threads_model": null, "threads_reasoning": null, "threads_in": "project"});
        assert!(
            rule(&shared, "sandfs")
                .ends_with("Every task works in this folder, with no worktree of its own.")
        );
    }

    fn root(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir()
            .join(format!("agent-app-project-{tag}-{}", std::process::id()))
            .join("my synthetic.repo");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// A daemon holding bots by name and workspace, refusing creates with
    /// `refuse` when set, and keeping every request.
    #[derive(Default)]
    struct Fake {
        bots: std::sync::Mutex<Vec<Value>>,
        calls: std::sync::Mutex<Vec<(String, Value)>>,
        refuse: Option<&'static str>,
    }

    impl Requests for Fake {
        async fn request(&self, op: &str, params: Value) -> crate::Result<Value> {
            self.calls.lock().unwrap().push((op.into(), params.clone()));
            let mut bots = self.bots.lock().unwrap();
            match op {
                "bots" => Ok(
                    json!({"bots": bots.iter().filter(|b| b["name"] == params["name"]).collect::<Vec<_>>(), "next_after": null}),
                ),
                "create" if self.refuse.is_some() => Err(crate::Error::new(self.refuse.unwrap())),
                "create" if bots.iter().any(|b| b["name"] == params["bot"]) => {
                    Err(crate::Error::new("bot_exists"))
                }
                "create" => {
                    let bot = json!({"name": params["bot"], "workspace": params["workspace"], "bot_id": bots.len() + 1});
                    bots.push(bot.clone());
                    Ok(bot)
                }
                _ => Err(crate::Error::new("unknown_op")),
            }
        }
    }

    fn make(daemon: &Fake, dir: &Path, policy: &Value, picks: &Picks) -> Result<Value, String> {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(create(
                daemon,
                dir,
                policy,
                &json!(["shell", "read"]),
                picks,
                "worktree",
                None,
            ))
    }

    #[test]
    fn a_new_project_makes_its_coordinator_then_its_file_once() {
        let dir = root("create");
        let daemon = Fake::default();
        let policy = json!({"instructions": "rules", "compaction_instructions": "keep", "tools": null, "model": null});
        let picks = Picks {
            model: Some("alpha/one".into()),
            effort: Some("high".into()),
            threads: Some(ThreadPicks {
                model: Some("beta/it's".into()),
                effort: Some("low".into()),
                in_project: true,
            }),
        };
        let made = make(&daemon, &dir, &policy, &picks).unwrap();
        assert_eq!(
            (&made["coordinator"], &made["created"], &made["from_file"]),
            (
                &json!("my-synthetic-repo.lead"),
                &json!(true),
                &json!(false)
            )
        );
        let calls = daemon.calls.lock().unwrap().clone();
        assert_eq!(
            calls.iter().map(|(op, _)| op.as_str()).collect::<Vec<_>>(),
            ["bots", "create"]
        );
        let create = &calls[1].1;
        assert_eq!(
            (
                &create["bot"],
                &create["workspace"],
                &create["model"],
                &create["effort"]
            ),
            (
                &json!("my-synthetic-repo.lead"),
                &json!(dir.to_str().unwrap()),
                &json!("alpha/one"),
                &json!("high")
            )
        );
        assert_eq!(
            create["instructions"],
            "rules\n\nThis project's tasks, as the person set them up: start every new task, in a role (--profile) or not, with --model 'beta/it'\\''s' --effort 'low'. Every task works in this folder, with no worktree of its own."
        );
        assert_eq!(
            (&create["tools"], &create["compaction_instructions"]),
            (&json!(["shell", "read"]), &json!("keep"))
        );
        let file = read(&dir).unwrap();
        assert_eq!(
            (
                &file["model"],
                &file["reasoning"],
                &file["threads_model"],
                &file["threads_in"]
            ),
            (
                &json!("alpha/one"),
                &json!("high"),
                &json!("beta/it's"),
                &json!("project")
            )
        );
        // Asked again, it is there: nothing is made or written.
        let again = make(
            &daemon,
            &dir,
            &policy,
            &Picks {
                model: Some("beta/two".into()),
                ..Picks::default()
            },
        )
        .unwrap();
        assert_eq!(again["created"], false);
        assert_eq!(again["record"]["name"], again["coordinator"]);
        assert_eq!(
            daemon
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(op, _)| op == "create")
                .count(),
            1
        );
        assert_eq!(read(&dir).unwrap()["model"], "alpha/one");
        // The same name from another folder is refused.
        let other = std::env::temp_dir()
            .join(format!(
                "agent-app-project-create-other-{}",
                std::process::id()
            ))
            .join("my synthetic.repo");
        std::fs::create_dir_all(&other).unwrap();
        let refused = make(&daemon, &other.canonicalize().unwrap(), &policy, &picks).unwrap_err();
        assert!(
            refused.starts_with("project_exists: my-synthetic-repo.lead already belongs to "),
            "{refused}"
        );
        assert!(!other.join(FILE).exists());
        std::fs::remove_dir_all(other.parent().unwrap()).unwrap();
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_project_needs_a_model_and_a_refused_one_is_never_written() {
        let dir = root("refused");
        let policy = json!({"instructions": "rules", "model": null});
        let daemon = Fake::default();
        assert!(
            make(&daemon, &dir, &policy, &Picks::default())
                .unwrap_err()
                .starts_with("model_required")
        );
        assert_eq!(
            daemon
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(op, _)| op == "create")
                .count(),
            0
        );
        let refusing = Fake {
            refuse: Some("model_unknown"),
            ..Fake::default()
        };
        let picks = Picks {
            model: Some("alpha/nope".into()),
            ..Picks::default()
        };
        assert!(
            make(&refusing, &dir, &policy, &picks)
                .unwrap_err()
                .starts_with("model_unknown")
        );
        assert!(
            !dir.join(FILE).exists(),
            "a model the daemon refuses is not saved"
        );
        // The role's model, when nothing is picked; its threads: the lead's own, in worktrees.
        let made = make(
            &daemon,
            &dir,
            &json!({"instructions": "rules", "model": "alpha/role"}),
            &Picks::default(),
        )
        .unwrap();
        assert_eq!(made["created"], true);
        let calls = daemon.calls.lock().unwrap().clone();
        let create = &calls.iter().find(|(op, _)| op == "create").unwrap().1;
        assert_eq!(create["model"], "alpha/role");
        assert!(
            create.get("settings").is_none(),
            "only Claude takes an hour"
        );
        assert!(
            create.get("effort").is_none(),
            "no effort picked sends none"
        );
        assert!(create["instructions"].as_str().unwrap().ends_with(r#"with --model "$AGENT_MODEL" ${AGENT_EFFORT:+--effort "$AGENT_EFFORT"}. When this folder is a git repository, every task works in its own worktree, so tasks do not collide: only you work in this folder."#));
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_folder_with_a_file_keeps_its_settings_over_the_picks() {
        let dir = root("kept");
        write(
            &dir,
            "demo",
            "alpha/one",
            Some("low"),
            &Threads {
                model: Some("beta/two"),
                reasoning: None,
                in_project: false,
            },
        )
        .unwrap();
        let daemon = Fake::default();
        let picks = Picks {
            model: Some("gamma/three".into()),
            effort: Some("max".into()),
            threads: None,
        };
        let made = make(&daemon, &dir, &json!({"instructions": "rules"}), &picks).unwrap();
        assert_eq!(
            (&made["project"], &made["from_file"]),
            (&json!("demo"), &json!(true))
        );
        let calls = daemon.calls.lock().unwrap().clone();
        let create = &calls[1].1;
        assert_eq!(
            (&create["bot"], &create["model"], &create["effort"]),
            (&json!("demo.lead"), &json!("alpha/one"), &json!("low"))
        );
        assert!(
            create["instructions"]
                .as_str()
                .unwrap()
                .contains("with --model 'beta/two'. When this folder")
        );
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_folder_without_a_file_offers_its_own_name() {
        let dir = root("default");
        let project = read(&dir).unwrap();
        assert_eq!(project["name"], "my-synthetic-repo");
        assert_eq!(project["coordinator"], "my-synthetic-repo.lead");
        assert_eq!(project["file"], false);
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_written_file_reads_back_and_is_never_overwritten() {
        let dir = root("write");
        let threads = Threads {
            model: Some("beta/two"),
            reasoning: Some("low"),
            in_project: true,
        };
        write(&dir, "demo", "alpha/one", Some("high"), &threads).unwrap();
        // One that appeared since the folder was read is kept and reported.
        let refused = write(&dir, "other", "beta/two", None, &Threads::default()).unwrap_err();
        assert!(refused.starts_with("project_changed: "), "{refused}");
        let project = read(&dir).unwrap();
        assert_eq!(project["name"], "demo");
        assert_eq!(project["coordinator"], "demo.lead");
        assert_eq!(project["model"], "alpha/one");
        assert_eq!(project["reasoning"], "high");
        assert_eq!(
            (
                &project["threads_model"],
                &project["threads_reasoning"],
                &project["threads_in"]
            ),
            (&json!("beta/two"), &json!("low"), &json!("project"))
        );
        assert_eq!(project["file"], true);
        assert!(write(&dir, "bad name", "alpha/one", None, &Threads::default()).is_err());
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_failed_write_leaves_no_file_and_no_temporary() {
        let dir = root("partial");
        std::fs::create_dir_all(dir.join(".agents")).unwrap();
        let path = dir.join(FILE);
        let error = place_new(&path, |file| {
            std::io::Write::write_all(file, b"name = \"de")?;
            Err(std::io::Error::other("synthetic disk full"))
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "synthetic disk full");
        assert!(!path.exists(), "no partial project file");
        // No threads' model: the coordinator picks, and they get worktrees.
        let alone = Threads {
            reasoning: Some("high"),
            ..Threads::default()
        };
        write(&dir, "demo", "alpha/one", None, &alone).unwrap();
        let project = read(&dir).unwrap();
        assert_eq!(
            (
                &project["name"],
                &project["reasoning"],
                &project["threads_model"],
                &project["threads_reasoning"],
                &project["threads_in"]
            ),
            (
                &json!("demo"),
                &Value::Null,
                &Value::Null,
                &Value::Null,
                &json!("worktree")
            )
        );
        let left: Vec<_> = std::fs::read_dir(dir.join(".agents"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, ["project.toml"], "temporaries are removed");
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn an_invalid_file_is_an_error_not_the_defaults() {
        let dir = root("invalid");
        std::fs::create_dir_all(dir.join(".agents")).unwrap();
        for text in [
            "name = ",
            "model = \"alpha/one\"",
            "name = 7",
            "name = \"demo\"\ncoordinator = \"other.lead\"",
            "name = \"demo\"\nthreads_in = \"elsewhere\"",
            "name = \"demo\"\nthreads_reasoning = \"low\"",
            "name = \"demo\"\nthreads_model = \"\"",
        ] {
            std::fs::write(dir.join(FILE), text).unwrap();
            let error = read(&dir).unwrap_err();
            assert!(error.starts_with("project_invalid: "), "{error}");
        }
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn an_unknown_key_is_refused_by_name() {
        let dir = root("unknown");
        std::fs::create_dir_all(dir.join(".agents")).unwrap();
        std::fs::write(dir.join(FILE), "name = \"demo\"\nrole = \"lead\"\n").unwrap();
        let error = read(&dir).unwrap_err();
        assert!(error.starts_with("project_invalid: "), "{error}");
        assert!(error.ends_with("unknown key role"), "{error}");
        std::fs::write(dir.join(FILE), "name = \"demo\"\nmodel = \"\"\n").unwrap();
        assert!(read(&dir).unwrap_err().ends_with("model must not be empty"));
        std::fs::write(dir.join(FILE), "name = \"demo\"\nreasoning = \"\"\n").unwrap();
        assert!(
            read(&dir)
                .unwrap_err()
                .ends_with("reasoning must not be empty")
        );
        std::fs::write(
            dir.join(FILE),
            "name = \"demo\"\ncoordinator = \"demo.lead\"\nmodel = \"alpha/one\"\n",
        )
        .unwrap();
        assert_eq!(read(&dir).unwrap()["model"], "alpha/one");
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }
}
