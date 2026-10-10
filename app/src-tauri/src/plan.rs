//! Plans. The `plan` skill's script keeps an agent's plan in
//! `STORE-plans/BOT_ID`, beside the daemon's store, which the agent's shell
//! names as `AGENT_STORE`; a bot id names one agent in one store, so the
//! folder is the store's and the file the agent's. The app reads them to show
//! the steps of the agents it shows, and removes one when its agent is deleted.
//! The text is the script's: one step a line, which the page reads.
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

/// The plans in `dir` of the agents `ids` names, by bot id, with null for one
/// that has none. A file that is not a plan's size or not text is not a plan.
pub fn read(dir: &Path, ids: &[i64]) -> Result<Value, String> {
    let mut out = Map::new();
    for &id in ids {
        let text = plain(&dir.join(id.to_string()))?;
        out.insert(id.to_string(), text.map_or(Value::Null, Value::String));
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
    fn plans_are_read_by_bot_id_and_what_is_not_one_is_none() {
        let root = scratch("read");
        let plans = root.join("state.sqlite-plans");
        assert_eq!(read(&plans, &[7]).unwrap(), serde_json::json!({"7": null}));
        std::fs::create_dir_all(&plans).unwrap();
        std::fs::write(plans.join("7"), "[x] Read it\n[>] Write it\n").unwrap();
        std::fs::write(plans.join("12"), "[ ] Ship it\n").unwrap();
        // One too big, one not text, and a folder.
        std::fs::write(plans.join("13"), vec![b'x'; CAP as usize + 1]).unwrap();
        std::fs::write(plans.join("14"), [0xff, 0xfe]).unwrap();
        std::fs::create_dir_all(plans.join("15")).unwrap();
        assert_eq!(
            read(&plans, &[7, 12, 13, 14, 15, 99]).unwrap(),
            serde_json::json!({"7": "[x] Read it\n[>] Write it\n", "12": "[ ] Ship it\n",
                "13": null, "14": null, "15": null, "99": null})
        );
        forget(&plans, 12).unwrap();
        forget(&plans, 12).unwrap();
        assert_eq!(
            read(&plans, &[12]).unwrap(),
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
        // Bash, macOS's sh, counts characters under a UTF-8 locale; dash counts bytes.
        let run_in = |shell: &str, locale: &str, args: &[&str]| {
            std::process::Command::new(shell)
                .arg(&script)
                .args(args)
                .env("LC_ALL", locale)
                .env("AGENT_STORE", std::fs::canonicalize(&store).unwrap())
                .env("AGENT_BOT_ID", "7")
                .output()
                .unwrap()
        };
        let run = |args: &[&str]| run_in("sh", "C", args);
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
            read(&dir(&store).unwrap(), &[7]).unwrap(),
            serde_json::json!({"7": "[x] Read it\n[>] Write it\n[ ] Ship it\n"})
        );
        assert_eq!(
            String::from_utf8_lossy(&wrote.stdout),
            "plan saved: 3 steps\n"
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
            read(&dir(&store).unwrap(), &[7]).unwrap(),
            serde_json::json!({"7": "[x] Read it\n[>] Write it\n[ ] Ship it\n"})
        );
        // 200 bytes is the limit whatever the locale counts: 67 three-byte characters are 201.
        let wide = run_in(
            "bash",
            "C.UTF-8",
            &["[x] Read it", &format!("[ ] {}", "€".repeat(66))],
        );
        assert_eq!(wide.status.code(), Some(2));
        let fits = run_in("bash", "C.UTF-8", &[&format!("[ ] {}", "€".repeat(65))]);
        assert!(fits.status.success());
        // --clear removes it, and clearing nothing is fine.
        for _ in 0..2 {
            let cleared = run(&["--clear"]);
            assert!(cleared.status.success());
            assert_eq!(String::from_utf8_lossy(&cleared.stdout), "plan cleared\n");
        }
        assert_eq!(
            read(&dir(&store).unwrap(), &[7]).unwrap(),
            serde_json::json!({"7": null})
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
