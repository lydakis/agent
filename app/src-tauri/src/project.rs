//! A project is a folder, its coordinator bot `NAME.lead`, and
//! `.agents/project.toml` in that folder. The file holds mechanics only (the
//! name, the coordinator's model and effort level, and its threads': their
//! model and effort, when the coordinator is not to pick, and whether they
//! work in their own worktrees or the project folder); how the coordinator
//! behaves stays in AGENTS.md and its profile, which tells it to read them. The project list itself comes from the coordinator bots in the
//! store, so this file is read when a project is opened and written once
//! when the app creates one. The daemon knows nothing of projects.
use serde_json::{Value, json};
use std::io::Read;
use std::path::Path;

pub const FILE: &str = ".agents/project.toml";
const LIMIT: u64 = 64 * 1024;
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
const THREADS_IN: [&str; 2] = ["worktree", "project"];

/// What a new project's threads run on and where they work.
#[derive(Debug, Default)]
pub struct Threads<'a> {
    pub model: Option<&'a str>,
    pub reasoning: Option<&'a str>,
    pub in_project: bool,
}

/// A name that is also a bot-name prefix: the daemon's name characters,
/// short enough that `NAME.lead` and its task names fit.
pub(crate) fn valid_name(name: &str) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn root(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir()
            .join(format!("agent-app-project-{tag}-{}", std::process::id()))
            .join("my synthetic.repo");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
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
