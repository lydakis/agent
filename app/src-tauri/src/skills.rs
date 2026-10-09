//! Skills the app ships. An agent reads only skills that are files in its
//! folder's `.agents/skills` or in `~/.agents/skills`, so the app puts each of
//! its own at `~/.agents/skills/NAME/SKILL.md`, where every agent created
//! afterwards finds it. That file is yours to edit or remove: the app keeps
//! what it last wrote there in `~/.agent/skills/NAME.md`, and replaces the
//! file with newer text only while it still matches. A folder's own skill of
//! the same name wins over it.
use std::path::Path;

pub const BUILT_IN: [(&str, &str); 1] = [(
    "automation",
    include_str!("../../skills/automation/SKILL.md"),
)];

/// Install or update every shipped skill under `home`; one that fails does
/// not stop the others.
pub fn install(home: &Path) -> Vec<String> {
    BUILT_IN
        .iter()
        .filter_map(|(name, text)| install_one(home, name, text).err())
        .collect()
}

fn install_one(home: &Path, name: &str, text: &str) -> Result<(), String> {
    let path = home.join(".agents/skills").join(name).join("SKILL.md");
    let written = home.join(".agent/skills").join(format!("{name}.md"));
    let read = |path: &Path| match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("{}: {error}", path.display())),
    };
    let have = read(&path)?;
    if have.as_deref() == Some(text) {
        // Current already; a start that ended before the record still owns it.
        if read(&written)?.as_deref() != Some(text) {
            crate::schedule::replace(&written, text)?;
        }
        return Ok(());
    }
    let last = read(&written)?;
    // Removed or edited since the app wrote it: yours.
    if last.is_some() && have != last {
        return Ok(());
    }
    // Mine first, then the record, so a crash between them is fixed above.
    crate::schedule::replace(&path, text)?;
    crate::schedule::replace(&written, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(tag: &str) -> std::path::PathBuf {
        let home =
            std::env::temp_dir().join(format!("agent-app-skills-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        home
    }

    #[test]
    fn a_shipped_skill_is_written_where_agents_index_it() {
        let home = home("fresh");
        assert!(install(&home).is_empty());
        let path = home.join(".agents/skills/automation/SKILL.md");
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, BUILT_IN[0].1);
        let skills = agent_client::policy::instructions(&home, None)
            .unwrap()
            .skills;
        assert!(
            skills
                .iter()
                .any(|s| s.name == "automation" && s.path == path)
        );
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn newer_text_replaces_the_apps_copy_and_never_yours() {
        let home = home("update");
        install_one(&home, "x", "old").unwrap();
        let path = home.join(".agents/skills/x/SKILL.md");
        install_one(&home, "x", "new").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");

        std::fs::write(&path, "mine").unwrap();
        install_one(&home, "x", "newer").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "mine");

        std::fs::remove_file(&path).unwrap();
        install_one(&home, "x", "newest").unwrap();
        assert!(!path.exists(), "a skill you removed stays removed");
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_start_that_ended_before_its_record_still_updates_later() {
        let home = home("crash");
        install_one(&home, "x", "old").unwrap();
        let path = home.join(".agents/skills/x/SKILL.md");
        // The file was replaced, then the app stopped before the record.
        std::fs::write(&path, "new").unwrap();
        install_one(&home, "x", "new").unwrap();
        install_one(&home, "x", "newer").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "newer");
        std::fs::remove_dir_all(home).unwrap();
    }
}
