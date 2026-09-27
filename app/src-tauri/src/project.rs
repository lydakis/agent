//! A project is a folder, its coordinator bot `NAME.lead`, and
//! `.agent/project.toml` in that folder. The file holds mechanics only (the
//! name and the coordinator's model); how the coordinator behaves stays in
//! AGENTS.md. The project list itself comes from the coordinator bots in the
//! store, so this file is read when a project is opened and written once
//! when the app creates one. The daemon knows nothing of projects.
use serde_json::{Value, json};
use std::io::Read;
use std::path::Path;

pub const FILE: &str = ".agent/project.toml";
const LIMIT: u64 = 64 * 1024;

/// A name that is also a bot-name prefix: the daemon's name characters,
/// short enough that `NAME.lead` and its task names fit.
fn valid_name(name: &str) -> bool {
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
            "model": null, "file": false,
        }));
    };
    let table: toml::Table = text
        .parse()
        .map_err(|e: toml::de::Error| invalid(e.message()))?;
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
    Ok(json!({
        "dir": dir, "name": name, "coordinator": coordinator, "model": model, "file": true,
    }))
}

/// Write a new project's file. An existing file is the user's and is kept.
pub fn write(dir: &Path, name: &str, model: &str) -> Result<(), String> {
    if !valid_name(name) {
        return Err("project_invalid: name must be 1-64 of A-Z a-z 0-9 - _ .".into());
    }
    let path = dir.join(FILE);
    let quote = |s: &str| toml::Value::String(s.to_owned()).to_string();
    let text = format!(
        "name = {}\ncoordinator = {}\nmodel = {}\n",
        quote(name),
        quote(&format!("{name}.lead")),
        quote(model)
    );
    let failed = |e: std::io::Error| format!("project_unwritable: {}: {e}", path.display());
    std::fs::create_dir_all(dir.join(".agent")).map_err(failed)?;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => std::io::Write::write_all(&mut file, text.as_bytes()).map_err(failed),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(failed(error)),
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
        write(&dir, "demo", "alpha/one").unwrap();
        write(&dir, "other", "beta/two").unwrap();
        let project = read(&dir).unwrap();
        assert_eq!(project["name"], "demo");
        assert_eq!(project["coordinator"], "demo.lead");
        assert_eq!(project["model"], "alpha/one");
        assert_eq!(project["file"], true);
        assert!(write(&dir, "bad name", "alpha/one").is_err());
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn an_invalid_file_is_an_error_not_the_defaults() {
        let dir = root("invalid");
        std::fs::create_dir_all(dir.join(".agent")).unwrap();
        for text in [
            "name = ",
            "model = \"alpha/one\"",
            "name = 7",
            "name = \"demo\"\ncoordinator = \"other.lead\"",
        ] {
            std::fs::write(dir.join(FILE), text).unwrap();
            let error = read(&dir).unwrap_err();
            assert!(error.starts_with("project_invalid: "), "{error}");
        }
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }
}
