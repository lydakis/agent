//! A project is a folder, its lead bot `NAME.lead`, and
//! `.agents/project.toml` in that folder, which holds mechanics only: the
//! name, its title (what the person calls it), the lead's model and effort level, and its tasks' model,
//! effort and speed and whether they work in their own worktrees or the project
//! folder, the profiles its lead and its tasks run as, and whether
//! the folders below it are the project's too. Clients
//! read it, and write it when they make a project; the
//! daemon knows nothing of projects.
use serde_json::{Value, json};
use std::io::Read;
use std::path::Path;

pub const FILE: &str = ".agents/project.toml";
/// The largest project file the reader takes.
pub const LIMIT: u64 = 64 * 1024;
/// Every key the file may hold; anything else is a mistake, not ignored.
const KEYS: [&str; 12] = [
    "name",
    "title",
    "lead",
    "model",
    "effort",
    "profile",
    "task_model",
    "task_effort",
    "task_speed",
    "task_profile",
    "task_in",
    "subfolders",
];
/// The migration that renames a project file's `threads_*` keys, as
/// 0.9 wrote them, to the `task_*` they are since 0.10.0 (`rename_task_keys`):
/// Sonny runs it once for each file, before it first reads it, until it
/// retires (Sonny's `upgrade::MIGRATIONS`).
pub const TASK_KEYS: &str = "project-task-keys";
/// The migration that renames a project file's `coordinator` key, as
/// 0.9 and the first 0.10.0 builds wrote it, to `lead` (`rename_lead_key`),
/// run as [`TASK_KEYS`] is.
pub const LEAD_KEY: &str = "project-lead-key";
/// The key before, and its name since.
const LEAD_RENAMED: [(&str, &str); 1] = [("coordinator", "lead")];
/// Each key before 0.10.0, and its name since.
const RENAMED: [(&str, &str); 5] = [
    ("threads_model", "task_model"),
    ("threads_effort", "task_effort"),
    ("threads_speed", "task_speed"),
    ("threads_profile", "task_profile"),
    ("threads_in", "task_in"),
];

/// `text`, the project file parsed as `table`, with the keys `keys`
/// names renamed in place, comments and order kept; none when it has
/// none, or when the renamed file would not read as the same settings
/// (a key in quotes, a line the rename does not find), which is then
/// refused as an unknown key.
fn renamed_keys(text: &str, table: &toml::Table, keys: &[(&str, &str)]) -> Option<String> {
    if !keys.iter().any(|(old, _)| table.contains_key(*old)) {
        return None;
    }
    let renamed: String = (text.split_inclusive('\n'))
        .map(|line| {
            let start = line.len() - line.trim_start().len();
            let rest = &line[start..];
            let found = keys.iter().find(|(old, _)| {
                rest.strip_prefix(old)
                    .is_some_and(|after| after.trim_start().starts_with('='))
            });
            match found {
                Some((old, new)) => format!("{}{new}{}", &line[..start], &rest[old.len()..]),
                None => line.to_owned(),
            }
        })
        .collect();
    let after: toml::Table = renamed.parse().ok()?;
    let mut expected = table.clone();
    for (old, new) in keys {
        if let Some(value) = expected.remove(*old)
            && expected.insert((*new).to_owned(), value).is_some()
        {
            return None;
        }
    }
    (after == expected).then_some(renamed)
}

/// The project file in `dir` with its `threads_*` keys renamed `task_*`
/// (`TASK_KEYS`), written in place: whether it renamed them. A file with
/// none renames nothing; one that does not parse, or would not read as the
/// same settings renamed, is left as it is, for `read` to refuse.
pub fn rename_task_keys(dir: &Path) -> bool {
    rename_keys(dir, &RENAMED)
}

/// The project file in `dir` with its `coordinator` key renamed `lead`
/// (`LEAD_KEY`), as [`rename_task_keys`] renames.
pub fn rename_lead_key(dir: &Path) -> bool {
    rename_keys(dir, &LEAD_RENAMED)
}

fn rename_keys(dir: &Path, renamed: &[(&str, &str)]) -> bool {
    let path = dir.join(FILE);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return false;
    };
    let Ok(table) = text.parse::<toml::Table>() else {
        return false;
    };
    let Some(renamed) = renamed_keys(&text, &table, renamed) else {
        return false;
    };
    let temp = path.with_extension(format!("toml.{}.tmp", std::process::id()));
    let written = std::fs::write(&temp, &renamed).and_then(|()| std::fs::rename(&temp, &path));
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    written.is_ok()
}

/// Where a project's tasks work: each in its own worktree, or all in
/// the project folder.
pub const TASK_IN: [&str; 2] = ["worktree", "project"];
/// What a folder below the project's is when no project file of its own
/// says otherwise: a project of its own, or this project's.
pub const SUBFOLDERS: [&str; 2] = ["own", "project"];

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
            "dir": dir, "name": name, "title": null, "lead": format!("{name}.lead"),
            "model": null, "effort": null, "profile": null, "task_model": null,
            "task_effort": null, "task_speed": null, "task_profile": null,
            "task_in": "worktree",
            "subfolders": "own", "file": false,
        }));
    };
    let table: toml::Table = text
        .parse()
        .map_err(|e: toml::de::Error| invalid(e.message()))?;
    if let Some(key) = table.keys().find(|key| !KEYS.contains(&key.as_str())) {
        let renamed = (RENAMED.iter().chain(&LEAD_RENAMED)).find(|(old, _)| old == key);
        return Err(invalid(&match (key.as_str(), renamed) {
            ("reasoning", _) => "unknown key reasoning: it is called effort now".to_owned(),
            (_, Some((old, new))) => format!("unknown key {old}: it is called {new} now"),
            _ => format!("unknown key {key}"),
        }));
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
    // What the person calls the project, theirs to change.
    let title = field("title")?.filter(|title| !title.trim().is_empty());
    let lead = format!("{name}.lead");
    if field("lead")?.is_some_and(|l| l != lead) {
        return Err(invalid(&format!("lead must be {lead}")));
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
    let (effort, task_model, task_effort) =
        (said("effort")?, said("task_model")?, said("task_effort")?);
    let task_speed = said("task_speed")?;
    if task_speed
        .as_deref()
        .is_some_and(|speed| !crate::config::SPEEDS.contains(&speed))
    {
        return Err(invalid(
            "task_speed must be \"standard\", \"fast\" or \"ultrafast\"",
        ));
    }
    // Profiles are looked up when the agents are made.
    let named = |key: &str| {
        let value = field(key)?;
        if value
            .as_deref()
            .is_some_and(|v| !crate::policy::profile_name(v))
        {
            return Err(invalid(&format!(
                "{key} must be a profile name: letters, digits, '-', '_' and '.'"
            )));
        }
        Ok(value)
    };
    let (profile, task_profile) = (named("profile")?, named("task_profile")?);
    let task_in = field("task_in")?.unwrap_or_else(|| "worktree".into());
    if !TASK_IN.contains(&task_in.as_str()) {
        return Err(invalid("task_in must be \"worktree\" or \"project\""));
    }
    let subfolders = field("subfolders")?.unwrap_or_else(|| "own".into());
    if !SUBFOLDERS.contains(&subfolders.as_str()) {
        return Err(invalid("subfolders must be \"own\" or \"project\""));
    }
    Ok(json!({
        "dir": dir, "name": name, "title": title, "lead": lead, "model": model,
        "effort": effort, "profile": profile, "task_model": task_model,
        "task_effort": task_effort, "task_speed": task_speed,
        "task_profile": task_profile, "task_in": task_in,
        "subfolders": subfolders, "file": true,
    }))
}

/// Where a project's tasks work, as its lead is told it; the client
/// that starts them picks their model and effort from the file.
pub fn tasks_rule(in_project: bool) -> String {
    match in_project {
        true => "Every task works in this folder, with no worktree of its own.",
        false => {
            "When this folder is a git repository, every task works in its own worktree, so tasks do not collide: only this folder's leads work in it. Elsewhere tasks share this folder, unless one is started in an empty folder of its own."
        }
    }
    .to_owned()
}

/// The task rule a project's file sets, as its lead is told it.
pub fn rule(info: &Value) -> String {
    tasks_rule(info["task_in"] == "project")
}

/// What a new project's tasks run on and where they work.
#[derive(Debug, Default)]
pub struct Tasks<'a> {
    pub model: Option<&'a str>,
    pub effort: Option<&'a str>,
    pub in_project: bool,
}

/// Write a new project's file. An existing file is the user's and is kept:
/// one that appeared since the folder was read is refused, to be read again.
pub fn write(
    dir: &Path,
    name: &str,
    title: Option<&str>,
    model: &str,
    effort: Option<&str>,
    tasks: &Tasks,
) -> Result<(), String> {
    if !valid_name(name) {
        return Err("project_invalid: name must be 1-64 of A-Z a-z 0-9 - _ .".into());
    }
    let path = dir.join(FILE);
    let quote = |s: &str| toml::Value::String(s.to_owned()).to_string();
    fn some(value: Option<&str>) -> Option<&str> {
        value.filter(|v| !v.is_empty())
    }
    let mut text = format!("name = {}\n", quote(name));
    if let Some(title) = some(title.map(str::trim)) {
        text.push_str(&format!("title = {}\n", quote(title)));
    }
    text.push_str(&format!(
        "lead = {}\nmodel = {}\n",
        quote(&format!("{name}.lead")),
        quote(model)
    ));
    if let Some(level) = some(effort) {
        text.push_str(&format!("effort = {}\n", quote(level)));
    }
    if let Some(model) = some(tasks.model) {
        text.push_str(&format!("task_model = {}\n", quote(model)));
    }
    if let Some(level) = some(tasks.effort) {
        text.push_str(&format!("task_effort = {}\n", quote(level)));
    }
    text.push_str(&format!(
        "task_in = {}\n",
        quote(TASK_IN[usize::from(tasks.in_project)])
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

/// What a project's creator asked for: its title, the coordinator's model
/// and effort, and its tasks' picks (none: they run as the client starting them
/// says, in worktrees). A folder whose file names a model keeps the file's
/// settings instead, and the picks are not used, except an effort the file
/// does not name.
#[derive(Debug, Default)]
pub struct Picks {
    /// What the person calls the project, written to a new file; a folder
    /// with a file keeps its own, or none.
    pub title: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub tasks: Option<TaskPicks>,
    /// The person's `web_search`, for a coordinator its role gives no tools.
    pub web_search: crate::config::WebSearch,
    /// The person's `web_search_off`, models it gets no search on.
    pub web_search_off: Vec<String>,
}

#[derive(Debug, Default)]
pub struct TaskPicks {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub in_project: bool,
}

/// A daemon that answers requests: this crate's `Client`, or a test's.
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
/// settings, then the folder's file when it had none, once the daemon took
/// the coordinator. `lead` names another lead to make in place of the
/// coordinator, for a folder with several. A project that already exists in this folder is
/// returned with `created: false`, its coordinator's row as the record, and
/// nothing changed; a name another
/// folder's coordinator holds is refused. A role that names no tools gets
/// the default set for its model, from `providers` (`ready.providers`).
pub async fn create(
    daemon: &impl Requests,
    dir: &Path,
    policy: &Value,
    providers: &Value,
    picks: &Picks,
    lead: Option<&str>,
) -> Result<Value, String> {
    let info = read(dir)?;
    let (name, coordinator) = (
        info["name"].as_str().unwrap_or_default(),
        lead.unwrap_or(info["lead"].as_str().unwrap_or_default()),
    );
    let at = dir
        .to_str()
        .ok_or("project_invalid: the folder must be valid UTF-8")?;
    let made = |created: bool, record: Value| {
        json!({"project": name, "lead": coordinator, "dir": at, "created": created,
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
    // A kept file that names no effort takes the picked one.
    let effort = match kept {
        true => info["effort"].as_str().or(picks.effort.as_deref()),
        false => picks.effort.as_deref(),
    };
    let rule = match file {
        true => rule(&info),
        false => tasks_rule(picks.tasks.as_ref().is_some_and(|t| t.in_project)),
    };
    let instructions = format!(
        "{}\n\n{rule}",
        policy["instructions"].as_str().unwrap_or_default()
    );
    // A lead starts a line of its own, whoever asked for it.
    let mut params = json!({
        "bot": coordinator, "workspace": at, "model": model,
        "instructions": instructions, "starts_line": true,
        "compaction_instructions": policy["compaction_instructions"],
        "tools": match &policy["tools"] {
            Value::Null => json!(crate::default_tools(providers, model, picks.web_search, &picks.web_search_off)),
            tools => tools.clone(),
        },
    });
    if let Some(effort) = effort {
        params["effort"] = json!(effort);
    }
    // The client's settings, such as a sandbox, with the model's.
    let mut settings = policy["settings"].as_object().cloned().unwrap_or_default();
    if let Some(ttl) = crate::policy::cache_ttl(model) {
        settings.insert("cache_ttl".into(), json!(ttl));
    }
    // The agents it lists and makes are its project's: named as it is up to
    // the first dot.
    if let Some((namespace, _)) = coordinator.split_once('.')
        && !namespace.is_empty()
    {
        settings
            .entry("namespace")
            .or_insert_with(|| json!(namespace));
    }
    if !settings.is_empty() {
        params["settings"] = Value::Object(settings);
    }
    // The profile its client chose for it by name.
    if !policy["profile"].is_null() {
        params["profile"] = policy["profile"].clone();
    }
    // The tools its client declares on the lead, from its first turn.
    if !policy["declare"].is_null() {
        params["declare"] = policy["declare"].clone();
    }
    // The client's gate, `create`'s approval fields, for the tools the lead
    // has: the client may not know which its model is given.
    if let (Value::Object(params), Some(gate)) = (&mut params, policy["gate"].as_object()) {
        let has = |name: &str| {
            params["tools"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|tool| tool == name)
                || (params.get("declare").and_then(Value::as_array))
                    .into_iter()
                    .flatten()
                    .any(|tool| tool["name"] == name)
        };
        let mut gate = gate.clone();
        if let Some(Value::Array(approve)) = gate.get_mut("approve") {
            approve.retain(|tool| tool.as_str().is_some_and(has));
            // Nothing left to gate is no gate, which `create` takes as
            // no fields: it refuses an empty `approve`.
            if approve.is_empty() {
                gate.remove("approve");
                gate.remove("approver");
                gate.remove("approve_expire_ms");
            }
        }
        params.extend(gate);
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
        let t = picks.tasks.as_ref();
        write(
            dir,
            name,
            picks.title.as_deref(),
            model,
            effort,
            &Tasks {
                model: t.and_then(|t| t.model.as_deref()),
                effort: t.and_then(|t| t.effort.as_deref()),
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
    fn the_rule_says_where_tasks_work() {
        let info = json!({"task_in": "worktree"});
        assert!(
            rule(&info)
                .ends_with("Elsewhere tasks share this folder, unless one is started in an empty folder of its own.")
        );
        let shared = json!({"task_in": "project"});
        assert!(
            rule(&shared)
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
                &json!({"alpha": {"family": "responses"}}),
                picks,
                None,
            ))
    }

    #[test]
    fn a_new_project_makes_its_coordinator_then_its_file_once() {
        let dir = root("create");
        let daemon = Fake::default();
        let policy = json!({"instructions": "rules", "compaction_instructions": "keep", "tools": null, "model": null});
        let picks = Picks {
            title: Some("Synthetic \"shop\" repo".into()),
            model: Some("alpha/one".into()),
            effort: Some("high".into()),
            tasks: Some(TaskPicks {
                model: Some("beta/it's".into()),
                effort: Some("low".into()),
                in_project: true,
            }),
            web_search: Default::default(),
            web_search_off: Vec::new(),
        };
        let made = make(&daemon, &dir, &policy, &picks).unwrap();
        assert_eq!(
            (&made["lead"], &made["created"], &made["from_file"]),
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
        // No fallback of its own: the daemon gives the person's.
        assert!(create.get("fallback").is_none());
        assert_eq!(
            create["instructions"],
            "rules\n\nEvery task works in this folder, with no worktree of its own."
        );
        assert_eq!(
            (&create["tools"], &create["compaction_instructions"]),
            (
                &json!([
                    "shell",
                    "write_stdin",
                    "read",
                    "write",
                    "apply_patch",
                    "wait",
                    "history",
                    "send_message",
                    "list_agents",
                    "pause_agents",
                    "spawn_agent",
                    "close_agent",
                    "web_fetch"
                ]),
                &json!("keep")
            )
        );
        let file = read(&dir).unwrap();
        assert_eq!(
            (
                &file["model"],
                &file["effort"],
                &file["task_model"],
                &file["task_effort"],
                &file["task_in"]
            ),
            (
                &json!("alpha/one"),
                &json!("high"),
                &json!("beta/it's"),
                &json!("low"),
                &json!("project")
            )
        );
        assert_eq!(file["title"], "Synthetic \"shop\" repo");
        // Asked again, it is there: nothing is made or written, the title
        // the person may have changed kept.
        let again = make(
            &daemon,
            &dir,
            &policy,
            &Picks {
                title: Some("Another title".into()),
                model: Some("beta/two".into()),
                ..Picks::default()
            },
        )
        .unwrap();
        assert_eq!(again["created"], false);
        assert_eq!(again["record"]["name"], again["lead"]);
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
        assert_eq!(read(&dir).unwrap()["title"], "Synthetic \"shop\" repo");
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
        // The role's model, when nothing is picked; its tasks in worktrees.
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
        // Only Claude takes an hour; the lead lists and makes its
        // project's agents, named as it is up to the first dot.
        let lead = made["lead"].as_str().unwrap();
        assert_eq!(
            create["settings"],
            json!({"namespace": lead.split_once('.').unwrap().0})
        );
        assert!(
            create.get("effort").is_none(),
            "no effort picked sends none"
        );
        assert!(create["instructions"].as_str().unwrap().ends_with("rules\n\nWhen this folder is a git repository, every task works in its own worktree, so tasks do not collide: only this folder's leads work in it. Elsewhere tasks share this folder, unless one is started in an empty folder of its own."));
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
        // A client's gate and settings go with the lead it makes.
        let dir = root("gated");
        let daemon = Fake::default();
        make(
            &daemon,
            &dir,
            &json!({"instructions": "rules", "model": "alpha/role",
                    "gate": {"approve": ["shell"], "approver": "auto"},
                    "settings": {"sandbox": "workspace-write", "network": true}}),
            &Picks::default(),
        )
        .unwrap();
        let calls = daemon.calls.lock().unwrap().clone();
        let create = &calls.iter().find(|(op, _)| op == "create").unwrap().1;
        assert_eq!(
            (&create["approve"], &create["approver"]),
            (&json!(["shell"]), &json!("auto"))
        );
        assert_eq!(
            create["settings"],
            json!({"sandbox": "workspace-write", "network": true, "namespace": "my-synthetic-repo"})
        );
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
        // No gate, as Sonny's `full` row gives, or one left with no tool
        // the lead has, is no gate fields: `create` refuses an empty
        // `approve` with `invalid_gate`.
        for gate in [
            json!({"approve": []}),
            json!({"approve": ["not_a_tool"], "approver": "auto"}),
        ] {
            let dir = root("ungated");
            let daemon = Fake::default();
            make(
                &daemon,
                &dir,
                &json!({"instructions": "rules", "model": "alpha/role", "gate": gate,
                        "settings": {"sandbox": "danger-full-access"}}),
                &Picks::default(),
            )
            .unwrap();
            let calls = daemon.calls.lock().unwrap().clone();
            let create = &calls.iter().find(|(op, _)| op == "create").unwrap().1;
            assert!(
                ["approve", "approver", "approve_expire_ms"]
                    .iter()
                    .all(|field| create.get(*field).is_none()),
                "{gate} sent {create}"
            );
            std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn a_folder_with_a_file_keeps_its_settings_over_the_picks() {
        let dir = root("kept");
        write(
            &dir,
            "demo",
            None,
            "alpha/one",
            Some("low"),
            &Tasks {
                model: Some("beta/two"),
                effort: None,
                in_project: false,
            },
        )
        .unwrap();
        let daemon = Fake::default();
        let picks = Picks {
            title: Some("Picked title".into()),
            model: Some("gamma/three".into()),
            effort: Some("max".into()),
            tasks: None,
            web_search: Default::default(),
            web_search_off: Vec::new(),
        };
        let made = make(&daemon, &dir, &json!({"instructions": "rules"}), &picks).unwrap();
        assert_eq!(
            (&made["project"], &made["from_file"]),
            (&json!("demo"), &json!(true))
        );
        // A folder that is a project already keeps its own title, here none.
        assert_eq!(read(&dir).unwrap()["title"], json!(null));
        let calls = daemon.calls.lock().unwrap().clone();
        let create = &calls[1].1;
        assert_eq!(
            (&create["bot"], &create["model"], &create["effort"]),
            (&json!("demo.lead"), &json!("alpha/one"), &json!("low"))
        );
        // A kept file that names no effort takes the picked one.
        let dir = root("kept-effort");
        write(&dir, "plain", None, "alpha/one", None, &Tasks::default()).unwrap();
        let daemon = Fake::default();
        make(&daemon, &dir, &json!({"instructions": "rules"}), &picks).unwrap();
        let calls = daemon.calls.lock().unwrap().clone();
        assert_eq!(
            (&calls[1].1["model"], &calls[1].1["effort"]),
            (&json!("alpha/one"), &json!("max"))
        );
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_folder_without_a_file_offers_its_own_name() {
        let dir = root("default");
        let project = read(&dir).unwrap();
        assert_eq!(project["name"], "my-synthetic-repo");
        assert_eq!(project["lead"], "my-synthetic-repo.lead");
        assert_eq!(project["file"], false);
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_written_file_reads_back_and_is_never_overwritten() {
        let dir = root("write");
        let tasks = Tasks {
            model: Some("beta/two"),
            effort: Some("low"),
            in_project: true,
        };
        write(&dir, "demo", None, "alpha/one", Some("high"), &tasks).unwrap();
        // One that appeared since the folder was read is kept and reported.
        let refused = write(&dir, "other", None, "beta/two", None, &Tasks::default()).unwrap_err();
        assert!(refused.starts_with("project_changed: "), "{refused}");
        let project = read(&dir).unwrap();
        assert_eq!(project["name"], "demo");
        assert_eq!(project["lead"], "demo.lead");
        assert_eq!(project["model"], "alpha/one");
        assert_eq!(project["effort"], "high");
        assert_eq!(
            (
                &project["task_model"],
                &project["task_effort"],
                &project["task_in"]
            ),
            (&json!("beta/two"), &json!("low"), &json!("project"))
        );
        assert_eq!(project["file"], true);
        // A file with no title gives none: the project shows its name.
        assert_eq!(project["title"], json!(null));
        assert!(write(&dir, "bad name", None, "alpha/one", None, &Tasks::default()).is_err());
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
        // A task effort needs no task model: theirs is the lead's.
        let alone = Tasks {
            effort: Some("high"),
            ..Tasks::default()
        };
        write(&dir, "demo", None, "alpha/one", None, &alone).unwrap();
        let project = read(&dir).unwrap();
        assert_eq!(
            (
                &project["name"],
                &project["effort"],
                &project["task_model"],
                &project["task_effort"],
                &project["task_in"]
            ),
            (
                &json!("demo"),
                &Value::Null,
                &Value::Null,
                &json!("high"),
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
    fn a_files_threads_keys_become_task_keys_in_their_own_step() {
        let dir = root("task-keys");
        std::fs::create_dir_all(dir.join(".agents")).unwrap();
        // No file yet: nothing to rename.
        assert!(!rename_task_keys(&dir));
        let before = "# mine\nname = \"demo\"\n  threads_model = \"a/b\" # pinned\nthreads_effort=\"high\"\nthreads_in = \"project\"\n";
        std::fs::write(dir.join(FILE), before).unwrap();
        // Reading alone renames nothing: the old key is refused by name.
        let error = read(&dir).unwrap_err();
        assert!(
            error.contains("unknown key threads_effort: it is called task_effort now"),
            "{error}"
        );
        assert_eq!(std::fs::read_to_string(dir.join(FILE)).unwrap(), before);
        assert!(rename_task_keys(&dir));
        assert_eq!(
            std::fs::read_to_string(dir.join(FILE)).unwrap(),
            "# mine\nname = \"demo\"\n  task_model = \"a/b\" # pinned\ntask_effort=\"high\"\ntask_in = \"project\"\n"
        );
        let info = read(&dir).unwrap();
        assert_eq!(
            (&info["task_model"], &info["task_effort"], &info["task_in"]),
            (&json!("a/b"), &json!("high"), &json!("project"))
        );
        // A renamed file has nothing left to rename.
        assert!(!rename_task_keys(&dir));
        // Both names at once read as two settings: left as it was.
        let both = "name = \"demo\"\nthreads_model = \"a/b\"\ntask_model = \"c/d\"\n";
        std::fs::write(dir.join(FILE), both).unwrap();
        assert!(!rename_task_keys(&dir));
        assert!(
            read(&dir)
                .unwrap_err()
                .contains("unknown key threads_model")
        );
        assert_eq!(std::fs::read_to_string(dir.join(FILE)).unwrap(), both);
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
            "name = \"demo\"\nlead = \"other.lead\"",
            "name = \"demo\"\ntask_in = \"elsewhere\"",
            "name = \"demo\"\ntask_effort = \"\"",
            "name = \"demo\"\nsubfolders = \"mine\"",
            "name = \"demo\"\ntask_model = \"\"",
            "name = \"demo\"\nprofile = \"../lead\"",
            "name = \"demo\"\ntask_profile = \"\"",
        ] {
            std::fs::write(dir.join(FILE), text).unwrap();
            let error = read(&dir).unwrap_err();
            assert!(error.starts_with("project_invalid: "), "{error}");
        }
        std::fs::write(dir.join(FILE), "name = \"demo\"\nsubfolders = \"project\"").unwrap();
        assert_eq!(read(&dir).unwrap()["subfolders"], "project");
        std::fs::write(
            dir.join(FILE),
            "name = \"demo\"\nprofile = \"my-lead\"\ntask_profile = \"worker\"",
        )
        .unwrap();
        let info = read(&dir).unwrap();
        assert_eq!(
            (&info["profile"], &info["task_profile"]),
            (&json!("my-lead"), &json!("worker"))
        );
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
        std::fs::write(dir.join(FILE), "name = \"demo\"\neffort = \"\"\n").unwrap();
        assert!(
            read(&dir)
                .unwrap_err()
                .ends_with("effort must not be empty")
        );
        // The key effort had before says its new name.
        std::fs::write(dir.join(FILE), "name = \"demo\"\nreasoning = \"high\"\n").unwrap();
        assert!(
            read(&dir)
                .unwrap_err()
                .ends_with("unknown key reasoning: it is called effort now")
        );
        std::fs::write(
            dir.join(FILE),
            "name = \"demo\"\nlead = \"demo.lead\"\nmodel = \"alpha/one\"\n",
        )
        .unwrap();
        assert_eq!(read(&dir).unwrap()["model"], "alpha/one");
        // The key the lead had before says its new name, and is renamed
        // in its own step.
        let before = "name = \"demo\"\ncoordinator = \"demo.lead\"\n";
        std::fs::write(dir.join(FILE), before).unwrap();
        assert!(
            read(&dir)
                .unwrap_err()
                .ends_with("unknown key coordinator: it is called lead now")
        );
        assert!(rename_lead_key(&dir));
        assert_eq!(
            std::fs::read_to_string(dir.join(FILE)).unwrap(),
            "name = \"demo\"\nlead = \"demo.lead\"\n"
        );
        assert_eq!(read(&dir).unwrap()["lead"], "demo.lead");
        assert!(!rename_lead_key(&dir));
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }
}
