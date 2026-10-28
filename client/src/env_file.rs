//! `~/.agent/env`: provider keys kept out of shell profiles, as `KEY=VALUE`
//! lines readable only by their owner. The clients (the app, Sonny) add them
//! over the environment they start a daemon with; neither the CLI nor the
//! daemon reads the file.
use std::path::{Path, PathBuf};

/// Keys, not documents: bounds the read that starts every daemon.
pub const MAX: u64 = 64 * 1024;

pub fn path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".agent/env"))
}

/// `KEY=VALUE` lines, `export` optional, `#` comments, and values optionally
/// in matching quotes. A missing file is empty. One others can read, or a line
/// that is not an assignment, is refused by path and line, never by value.
pub fn read(path: &Path) -> Result<Vec<(String, String)>, String> {
    use std::{io::Read, os::unix::fs::PermissionsExt};
    let unreadable =
        |error: std::io::Error| format!("env_file_unreadable: {}: {error}", path.display());
    // Checked before opening: opening a FIFO for reading would block.
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(unreadable(error)),
    };
    if !metadata.is_file() || metadata.len() > MAX {
        return Err(format!(
            "env_file_invalid: {} must be a regular file of at most {MAX} bytes",
            path.display()
        ));
    }
    let mut text = String::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(MAX + 1).read_to_string(&mut text))
        .map_err(unreadable)?;
    if text.len() as u64 > MAX {
        return Err(format!(
            "env_file_invalid: {} must be a regular file of at most {MAX} bytes",
            path.display()
        ));
    }
    let mode = metadata.permissions().mode();
    if mode & 0o077 != 0 {
        return Err(format!(
            "env_file_permissions: {} is readable by others; chmod 600 it",
            path.display()
        ));
    }
    let mut environment = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let valid = |key: &str| {
            key.chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        };
        let Some((key, value)) = line
            .split_once('=')
            .map(|(key, value)| (key.trim_end(), value))
            .filter(|(key, _)| valid(key))
        else {
            return Err(format!(
                "env_file_invalid: {} line {}: expected KEY=VALUE",
                path.display(),
                index + 1
            ));
        };
        let value = value.trim();
        let value = ['"', '\'']
            .iter()
            .find_map(|quote| {
                value
                    .strip_prefix(*quote)
                    .and_then(|inner| inner.strip_suffix(*quote))
            })
            .unwrap_or(value);
        environment.push((key.to_owned(), value.to_owned()));
    }
    Ok(environment)
}

/// What a program Sonny starts sees as `name`: the file's value over this
/// process's own, as it is applied to them, an empty one unsetting it.
pub fn var(name: &str) -> Option<String> {
    let kept = path()
        .and_then(|file| read(&file).ok())
        .and_then(|pairs| pairs.into_iter().rev().find(|(key, _)| key == name));
    match kept {
        Some((_, value)) => Some(value),
        None => std::env::var(name).ok(),
    }
    .filter(|value| !value.is_empty())
}

/// Sets each of `pairs` in the file at `path`, made readable only by its
/// owner when it is new: earlier lines naming one of them are dropped,
/// every other line is kept as it was, and the file is replaced whole.
pub fn set(path: &Path, pairs: &[(&str, &str)]) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    // A file this refuses to read is the person's to fix, not to replace.
    read(path)?;
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("env_file_unreadable: {}: {error}", path.display())),
    };
    let named = |line: &str| {
        let line = line.trim();
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        line.split_once('=')
            .is_some_and(|(key, _)| pairs.iter().any(|(name, _)| *name == key.trim_end()))
    };
    let mut kept: String = text
        .lines()
        .filter(|line| !named(line))
        .flat_map(|line| [line, "\n"])
        .collect();
    for (key, value) in pairs {
        if value.contains(['\n', '\r']) {
            return Err(format!("env_file_invalid: {key} holds a line break"));
        }
        kept.push_str(&format!("{key}={}\n", value.trim()));
    }
    if kept.len() as u64 > MAX {
        return Err(format!(
            "env_file_invalid: {} must be a regular file of at most {MAX} bytes",
            path.display()
        ));
    }
    let failed =
        |error: std::io::Error| format!("env_file_unwritable: {}: {error}", path.display());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(failed)?;
    }
    let staged = path.with_file_name(format!(".env.{}", std::process::id()));
    let written = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&staged)?;
        file.write_all(kept.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&staged, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    written.map_err(failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_env_file_takes_assignments_and_refuses_anything_else() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("agent-client-env-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("env");
        assert_eq!(read(&file).unwrap(), vec![]);
        std::fs::write(
            &file,
            "# keys\n\nOPENAI_API_KEY=sk-1\nexport ANTHROPIC_API_KEY = \"a b\"\nX='q'\nEMPTY=\nURL=a=b\n",
        )
        .unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let pairs = |list: &[(&str, &str)]| {
            list.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            read(&file).unwrap(),
            pairs(&[
                ("OPENAI_API_KEY", "sk-1"),
                ("ANTHROPIC_API_KEY", "a b"),
                ("X", "q"),
                ("EMPTY", ""),
                ("URL", "a=b"),
            ])
        );
        std::fs::write(&file, "OK=1\nsk-secret\n").unwrap();
        let error = read(&file).unwrap_err();
        assert!(
            error.starts_with("env_file_invalid: ")
                && error.ends_with("line 2: expected KEY=VALUE"),
            "{error}"
        );
        assert!(!error.contains("sk-secret"));
        std::fs::write(&file, "1X=1\n").unwrap();
        assert!(read(&file).is_err());
        std::fs::write(&file, "X=".repeat(40_000)).unwrap();
        assert!(read(&file).unwrap_err().starts_with("env_file_invalid: "));
        std::fs::remove_file(&file).unwrap();
        std::fs::create_dir(&file).unwrap();
        assert!(read(&file).unwrap_err().starts_with("env_file_invalid: "));
        std::fs::remove_dir(&file).unwrap();
        std::fs::write(&file, "OK=1\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            read(&file)
                .unwrap_err()
                .starts_with("env_file_permissions: ")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn setting_keys_replaces_theirs_and_keeps_every_other_line() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("agent-client-set-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let file = root.join("env");
        set(&file, &[("AGENT_PROVIDER", "bedrock")]).unwrap();
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::write(&file, "# mine\nexport AGENT_MODEL=a/b\nOTHER=1\n").unwrap();
        set(&file, &[("AGENT_MODEL", "c/d"), ("AWS_PROFILE", " dev ")]).unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "# mine\nOTHER=1\nAGENT_MODEL=c/d\nAWS_PROFILE=dev\n"
        );
        assert!(set(&file, &[("KEY", "a\nb")]).is_err());
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            set(&file, &[("KEY", "1")])
                .unwrap_err()
                .starts_with("env_file_permissions: ")
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
