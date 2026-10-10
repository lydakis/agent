//! Plans. The `plan` skill's script keeps an agent's plan in
//! `STORE-plans/BOT_ID`, beside the daemon's store, which the agent's shell
//! names as `AGENT_STORE`; a bot id names one agent in one store, so the
//! folder is the store's and the file the agent's. The app reads them to show
//! each agent's steps, and removes one when its agent is deleted. The text is
//! the script's: one step a line, which the page reads.
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// A plan is at most 30 steps of 200 bytes; a file past this is not one.
pub const CAP: u64 = 16 * 1024;

/// The plans of the store at `store`, as the daemon names it to its agents:
/// its canonical path, with `-plans` after it.
pub fn dir(store: &Path) -> Result<PathBuf, String> {
    let mut name = std::fs::canonicalize(store)
        .map_err(|e| format!("{}: {e}", store.display()))?
        .into_os_string();
    name.push("-plans");
    Ok(PathBuf::from(name))
}

/// The plans in `dir` by bot id: every one there, or `ids`' with null for
/// one that has none. A file named for no bot id, or one that is not a
/// plan's size or not text, is not a plan.
pub fn read(dir: &Path, ids: Option<&[i64]>) -> Result<Value, String> {
    let mut out = Map::new();
    let one = |id: i64| plain(&dir.join(id.to_string()));
    match ids {
        Some(ids) => {
            for &id in ids {
                out.insert(id.to_string(), one(id)?.map_or(Value::Null, Value::String));
            }
        }
        None => {
            let entries = match std::fs::read_dir(dir) {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(Value::Object(out));
                }
                Err(e) => return Err(format!("{}: {e}", dir.display())),
            };
            for entry in entries {
                let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
                let Some(id) = entry
                    .file_name()
                    .to_str()
                    .and_then(|n| n.parse::<i64>().ok())
                else {
                    continue;
                };
                if let Some(text) = one(id)? {
                    out.insert(id.to_string(), Value::String(text));
                }
            }
        }
    }
    Ok(Value::Object(out))
}

fn plain(path: &Path) -> Result<Option<String>, String> {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    if !meta.is_file() || meta.len() > CAP {
        return Ok(None);
    }
    match std::fs::read(path) {
        Ok(bytes) => Ok(String::from_utf8(bytes).ok()),
        // Replaced or removed between the look and the read: the next read sees what is there.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Remove a deleted agent's plan; one it never wrote is already gone.
pub fn forget(dir: &Path, id: i64) -> Result<(), String> {
    match std::fs::remove_file(dir.join(id.to_string())) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(format!("{}: {e}", dir.display()))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("agent-app-plan-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn plans_sit_beside_the_canonical_store() {
        let root = scratch("dir");
        std::fs::write(root.join("state.sqlite"), "").unwrap();
        let through = root.join("sub/..").join("state.sqlite");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let canonical = std::fs::canonicalize(&root).unwrap();
        assert_eq!(dir(&through).unwrap(), canonical.join("state.sqlite-plans"));
        assert!(dir(&root.join("missing.sqlite")).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn every_plan_is_read_by_bot_id_and_what_is_not_one_is_skipped() {
        let root = scratch("read");
        let plans = root.join("state.sqlite-plans");
        assert_eq!(read(&plans, None).unwrap(), serde_json::json!({}));
        std::fs::create_dir_all(&plans).unwrap();
        std::fs::write(plans.join("7"), "[x] Read it\n[>] Write it\n").unwrap();
        std::fs::write(plans.join("12"), "[ ] Ship it\n").unwrap();
        // The script's temporary, a file named for no bot, one too big, and one not text.
        std::fs::write(plans.join(".plan.ab12cd"), "[ ] half").unwrap();
        std::fs::write(plans.join("notes"), "[ ] no").unwrap();
        std::fs::write(plans.join("13"), vec![b'x'; CAP as usize + 1]).unwrap();
        std::fs::write(plans.join("14"), [0xff, 0xfe]).unwrap();
        std::fs::create_dir_all(plans.join("15")).unwrap();
        assert_eq!(
            read(&plans, None).unwrap(),
            serde_json::json!({"7": "[x] Read it\n[>] Write it\n", "12": "[ ] Ship it\n"})
        );
        assert_eq!(
            read(&plans, Some(&[12, 99])).unwrap(),
            serde_json::json!({"12": "[ ] Ship it\n", "99": null})
        );
        forget(&plans, 12).unwrap();
        forget(&plans, 12).unwrap();
        assert_eq!(
            read(&plans, Some(&[12])).unwrap(),
            serde_json::json!({"12": null})
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_skill_script_writes_what_is_read() {
        let root = scratch("script");
        let store = root.join("state.sqlite");
        std::fs::write(&store, "").unwrap();
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../skills/plan/plan");
        let run = |args: &[&str]| {
            std::process::Command::new("sh")
                .arg(&script)
                .args(args)
                .env("AGENT_STORE", std::fs::canonicalize(&store).unwrap())
                .env("AGENT_BOT_ID", "7")
                .output()
                .unwrap()
        };
        let shown = run(&[]);
        assert!(shown.status.success());
        assert_eq!(String::from_utf8_lossy(&shown.stdout), "no plan yet\n");
        let wrote = run(&["[x] Read it", "[>] Write it", "[ ] Ship it"]);
        assert!(
            wrote.status.success(),
            "{}",
            String::from_utf8_lossy(&wrote.stderr)
        );
        assert_eq!(
            read(&dir(&store).unwrap(), None).unwrap(),
            serde_json::json!({"7": "[x] Read it\n[>] Write it\n[ ] Ship it\n"})
        );
        // A step without its mark, or on two lines, or too long, changes nothing.
        for bad in [
            "Write it",
            "[x] a\nb",
            "[ ] ",
            &format!("[ ] {}", "x".repeat(200)),
        ] {
            let refused = run(&["[x] Read it", bad]);
            assert_eq!(refused.status.code(), Some(2), "{bad}");
        }
        let many: Vec<String> = (0..31).map(|i| format!("[ ] Step {i}")).collect();
        assert_eq!(
            run(&many.iter().map(String::as_str).collect::<Vec<_>>())
                .status
                .code(),
            Some(2)
        );
        assert_eq!(
            read(&dir(&store).unwrap(), Some(&[7])).unwrap(),
            serde_json::json!({"7": "[x] Read it\n[>] Write it\n[ ] Ship it\n"})
        );
        // Outside an agent's shell it says so.
        let outside = std::process::Command::new("sh")
            .arg(&script)
            .env_remove("AGENT_STORE")
            .output()
            .unwrap();
        assert_eq!(outside.status.code(), Some(2));
        std::fs::remove_dir_all(root).unwrap();
    }
}
