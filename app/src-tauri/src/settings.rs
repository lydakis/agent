//! What the app keeps for the daemons it starts. `~/.agent/env`, which
//! `daemon` applies over the login shell's environment on every start, holds
//! the providers a daemon runs (`AGENT_PROVIDER`) and what they need (keys,
//! the AWS region and profile). There is no default model to keep: each
//! project or agent is given its own when it is made.
//! The page reads what is set and writes new values; a key's value never
//! goes back to the page. A change reaches the daemon when it next starts.
use serde_json::{Map, Value, json};
use std::ffi::OsString;
use std::path::Path;

/// Settings the page may read back as values.
const PLAIN: [&str; 3] = ["AGENT_PROVIDER", "AWS_REGION", "AWS_PROFILE"];
/// Keys the page may set, and learn only whether they are set.
const SECRET: [&str; 4] = [
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "OPENROUTER_API_KEY",
    "AWS_BEARER_TOKEN_BEDROCK",
];
/// The providers a start without `AGENT_PROVIDER` detects, as the CLI does.
const DETECTED: [(&str, &str); 3] = [
    ("anthropic", "ANTHROPIC_API_KEY"),
    ("openai", "OPENAI_API_KEY"),
    ("openrouter", "OPENROUTER_API_KEY"),
];

/// What a daemon this app starts would be given: the file's values over the
/// login shell's. `providers` are the specs it would run.
pub fn view(file: &[(String, String)], login: Option<&[(OsString, OsString)]>) -> Value {
    let get = |name: &str| {
        file.iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
            .or_else(|| {
                login?
                    .iter()
                    .find(|(key, _)| key == name)
                    .and_then(|(_, value)| value.to_str().map(str::to_owned))
            })
            .filter(|value| !value.trim().is_empty())
    };
    let providers: Vec<String> = match get("AGENT_PROVIDER") {
        Some(specs) => specs.split_whitespace().map(str::to_owned).collect(),
        None => DETECTED
            .iter()
            .filter(|(_, key)| get(key).is_some())
            .map(|(name, _)| (*name).to_owned())
            .collect(),
    };
    let keys: Vec<&str> = SECRET
        .into_iter()
        .filter(|key| get(key).is_some())
        .collect();
    json!({
        "providers": providers,
        "region": get("AWS_REGION").or_else(|| get("AWS_DEFAULT_REGION")),
        "profile": get("AWS_PROFILE"),
        "keys": keys,
    })
}

/// The name a line assigns, if it is an assignment.
fn assigned(line: &str) -> Option<&str> {
    let line = line.trim();
    let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
    let (key, _) = line.split_once('=')?;
    Some(key.trim_end())
}

/// `text` with each named setting set to its value, or removed for null, so
/// the login shell's applies again. An empty value is written empty: a start
/// unsets it, clearing what the shell exports (a profile, or a key it would
/// detect as a provider). Every other line is kept as it was, and a setting
/// given twice keeps only its first place.
pub fn edit(text: &str, changes: &Map<String, Value>) -> Result<String, String> {
    let mut wanted = Vec::new();
    for (key, value) in changes {
        if !PLAIN.contains(&key.as_str()) && !SECRET.contains(&key.as_str()) {
            return Err(format!("settings_invalid: {key} is not a setting"));
        }
        let value = match value {
            Value::Null => None,
            Value::String(value) => Some(value.trim()),
            _ => return Err(format!("settings_invalid: {key} must be text")),
        };
        // One line per setting: a line break would start another assignment.
        if value.is_some_and(|value| value.contains(['\n', '\r', '\0'])) {
            return Err(format!("settings_invalid: {key} must be one line"));
        }
        wanted.push((key.as_str(), value));
    }
    // Quoted, so the reader returns the value exactly: it strips one pair.
    let line = |key: &str, value: &str| format!("{key}=\"{value}\"\n");
    let mut out = String::new();
    let mut placed = Vec::new();
    for current in text.lines() {
        match assigned(current).and_then(|key| wanted.iter().find(|(k, _)| *k == key)) {
            None => {
                out.push_str(current);
                out.push('\n');
            }
            Some((key, value)) => {
                if !placed.contains(key) {
                    placed.push(key);
                    if let Some(value) = value {
                        out.push_str(&line(key, value));
                    }
                }
            }
        }
    }
    for (key, value) in &wanted {
        if let (false, Some(value)) = (placed.contains(key), value) {
            out.push_str(&line(key, value));
        }
    }
    Ok(out)
}

/// Replace `path` whole: written beside it, synced, then renamed over it, so
/// a failed write leaves the old file. `mode` applies from creation, so a
/// key is never readable by others, not even briefly.
pub fn replace(path: &Path, text: &str, mode: u32) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let failed =
        |error: std::io::Error| format!("settings_unwritable: {}: {error}", path.display());
    let dir = path.parent().ok_or("settings_unwritable: no folder")?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(failed)?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let staged = dir.join(format!(".{name}.{}.{n}", std::process::id()));
    let written = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&staged)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&staged, path)?;
        // The new name is durable only once its folder is.
        std::fs::File::open(dir)?.sync_all()
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    written.map_err(failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changes(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn the_view_is_the_files_values_over_the_login_shells_and_keys_are_only_named() {
        let file = vec![
            ("AWS_REGION".to_owned(), "eu-west-1".to_owned()),
            ("ANTHROPIC_API_KEY".to_owned(), "sk-file".to_owned()),
        ];
        let login = vec![
            (OsString::from("AWS_REGION"), OsString::from("us-east-1")),
            (OsString::from("OPENAI_API_KEY"), OsString::from("sk-login")),
        ];
        let seen = view(&file, Some(&login));
        assert_eq!(seen["region"], "eu-west-1");
        assert!(seen.get("model").is_none());
        assert_eq!(seen["providers"], json!(["anthropic", "openai"]));
        assert_eq!(seen["keys"], json!(["ANTHROPIC_API_KEY", "OPENAI_API_KEY"]));
        assert!(!seen.to_string().contains("sk-"));
        // An empty key in the file hides the shell's, so nothing is detected.
        let hidden = vec![("OPENAI_API_KEY".to_owned(), String::new())];
        assert_eq!(view(&hidden, Some(&login))["providers"], json!([]));
        assert_eq!(view(&hidden, Some(&login))["keys"], json!([]));
        // Named providers replace detection, as they do for the CLI.
        let named = vec![("AGENT_PROVIDER".to_owned(), " bedrock  chatgpt ".to_owned())];
        assert_eq!(
            view(&named, Some(&login))["providers"],
            json!(["bedrock", "chatgpt"])
        );
        let fallback = vec![(
            OsString::from("AWS_DEFAULT_REGION"),
            OsString::from("ap-south-1"),
        )];
        assert_eq!(view(&[], Some(&fallback))["region"], "ap-south-1");
        assert_eq!(view(&[], None)["providers"], json!([]));
    }

    #[test]
    fn an_edit_sets_and_removes_settings_and_keeps_every_other_line() {
        let text = "# mine\nexport AWS_REGION=us-east-1\nOTHER=1\nAWS_REGION=eu-west-1\nOPENAI_API_KEY='sk-old'\n";
        let edited = edit(
            text,
            &changes(json!({
                "AWS_REGION": "us-west-2",
                "OPENAI_API_KEY": null,
                "AGENT_PROVIDER": "bedrock chatgpt",
                "AWS_PROFILE": null,
                "ANTHROPIC_API_KEY": "  ",
            })),
        )
        .unwrap();
        assert_eq!(
            edited,
            "# mine\nAWS_REGION=\"us-west-2\"\nOTHER=1\nAGENT_PROVIDER=\"bedrock chatgpt\"\nANTHROPIC_API_KEY=\"\"\n"
        );
        // What is written reads back exactly, whatever quotes the value holds.
        let path = std::env::temp_dir().join(format!("agent-app-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        let file = path.join("env");
        let odd = edit("", &changes(json!({"AWS_PROFILE": "\"work\" 'x'"}))).unwrap();
        replace(&file, &odd, 0o600).unwrap();
        let read = crate::daemon::read_env_file(&file).unwrap();
        assert_eq!(
            read,
            [("AWS_PROFILE".to_owned(), "\"work\" 'x'".to_owned())]
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(std::fs::read_dir(&path).unwrap().count(), 1);
        std::fs::remove_dir_all(&path).unwrap();
    }

    #[test]
    fn an_edit_refuses_what_is_not_a_setting_or_not_one_line() {
        for bad in [
            json!({"PATH": "/bin"}),
            json!({"AGENT_MODEL": "openai/gpt-6-luna"}),
            json!({"AWS_REGION": 1}),
            json!({"OPENAI_API_KEY": "sk\nAGENT_PROVIDER=x"}),
        ] {
            assert!(
                edit("", &changes(bad))
                    .unwrap_err()
                    .starts_with("settings_invalid")
            );
        }
    }
}
