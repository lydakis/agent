//! Skills the app ships. An agent reads only skills in its folder's
//! `.agents/skills` or in `~/.agents/skills`, so the app links each skill in
//! its bundle's `Contents/Resources/skills/NAME` from `~/.agents/skills/NAME`.
//! A link is never stale: updating the app updates what it points at, with
//! nothing copied or recorded. A folder or other file of yours at that name
//! is left alone, and a folder's own skill of the same name wins over it.
use std::path::{Path, PathBuf};

/// Where a bundled app keeps its skills, beside `Contents/MacOS/BINARY`.
pub fn bundled(exe: &Path) -> Option<PathBuf> {
    let skills = exe.parent()?.parent()?.join("Resources/skills");
    skills.is_dir().then_some(skills)
}

/// A link the app made: one to `SOMETHING.app/Contents/Resources/skills/NAME`.
fn ours(target: &Path) -> bool {
    let mut up = target.ancestors().skip(1).map(Path::file_name);
    matches!(
        (up.next(), up.next(), up.next(), up.next()),
        (Some(Some(a)), Some(Some(b)), Some(Some(c)), Some(Some(app)))
            if a == "skills" && b == "Resources" && c == "Contents"
                && app.to_string_lossy().ends_with(".app")
    )
}

/// Link every skill in `shipped` from `home`'s `~/.agents/skills`, point the
/// app's links that lead elsewhere (an app since moved) here, and remove the
/// app's links to skills it no longer ships. One that fails does not stop
/// the others.
pub fn install(home: &Path, shipped: &Path) -> Vec<String> {
    let dir = home.join(".agents/skills");
    let mut errors = Vec::new();
    let mut fail = |path: &Path, error: std::io::Error| {
        errors.push(format!("{}: {error}", path.display()));
    };
    let names: Vec<_> = match std::fs::read_dir(shipped) {
        Ok(entries) => entries
            .flatten()
            .filter(|entry| entry.path().join("SKILL.md").is_file())
            .map(|entry| entry.file_name())
            .collect(),
        Err(error) => {
            fail(shipped, error);
            return errors;
        }
    };
    if let Err(error) = std::fs::create_dir_all(&dir) {
        fail(&dir, error);
        return errors;
    }
    for name in &names {
        let (link, target) = (dir.join(name), shipped.join(name));
        let relink = match std::fs::read_link(&link) {
            Ok(current) => current != target && ours(&current),
            // Nothing there: link it. A folder or file of yours: leave it.
            Err(_) => std::fs::symlink_metadata(&link).is_err(),
        };
        if relink {
            let made = match std::fs::remove_file(&link) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
                _ => std::os::unix::fs::symlink(&target, &link),
            };
            if let Err(error) = made {
                fail(&link, error);
            }
        }
    }
    // A skill no longer shipped leaves the app's link dangling, and the
    // skill index refuses a dangling link, so it goes. The index reads no
    // more of the folder than its own bound, nor does this.
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return errors;
    };
    for entry in entries.take(4096).flatten() {
        let link = entry.path();
        if names.contains(&entry.file_name()) {
            continue;
        }
        if let Ok(target) = std::fs::read_link(&link)
            && ours(&target)
            && std::fs::metadata(&link).is_err()
            && let Err(error) = std::fs::remove_file(&link)
        {
            fail(&link, error);
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A home and a bundle `Agent.app` shipping `names`.
    fn setup(tag: &str, names: &[&str]) -> (PathBuf, PathBuf) {
        let root =
            std::env::temp_dir().join(format!("agent-app-skills-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let shipped = root.join("Agent.app/Contents/Resources/skills");
        for name in names {
            std::fs::create_dir_all(shipped.join(name)).unwrap();
            std::fs::write(shipped.join(name).join("SKILL.md"), name).unwrap();
        }
        (root.join("home"), shipped)
    }

    fn read(home: &Path, name: &str) -> String {
        std::fs::read_to_string(home.join(".agents/skills").join(name).join("SKILL.md")).unwrap()
    }

    #[test]
    fn a_bundle_finds_its_skills_beside_its_binary() {
        let (home, shipped) = setup("bundled", &["automation"]);
        let exe = shipped.join("../../MacOS/agent-app");
        assert_eq!(
            bundled(&exe).unwrap(),
            shipped.join("../../Resources/skills")
        );
        assert!(bundled(&home.join("agent-app")).is_none());
        std::fs::remove_dir_all(home.parent().unwrap()).unwrap();
    }

    #[test]
    fn each_shipped_skill_is_linked_and_follows_the_app() {
        let (home, shipped) = setup("linked", &["automation"]);
        assert!(install(&home, &shipped).is_empty());
        assert_eq!(read(&home, "automation"), "automation");
        // An update changes the bundle in place; the link reads the new text.
        std::fs::write(shipped.join("automation/SKILL.md"), "newer").unwrap();
        assert_eq!(read(&home, "automation"), "newer");
        assert!(install(&home, &shipped).is_empty());
        std::fs::remove_dir_all(home.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_moved_app_takes_its_links_along() {
        let (home, shipped) = setup("moved", &["automation"]);
        install(&home, &shipped);
        let root = home.parent().unwrap();
        std::fs::create_dir_all(root.join("Applications")).unwrap();
        std::fs::rename(root.join("Agent.app"), root.join("Applications/Agent.app")).unwrap();
        let moved = root.join("Applications/Agent.app/Contents/Resources/skills");
        assert!(install(&home, &moved).is_empty());
        assert_eq!(
            std::fs::read_link(home.join(".agents/skills/automation")).unwrap(),
            moved.join("automation")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn your_folder_or_link_of_the_same_name_stays() {
        let (home, shipped) = setup("yours", &["automation", "memory"]);
        let skills = home.join(".agents/skills");
        std::fs::create_dir_all(skills.join("automation")).unwrap();
        std::fs::write(skills.join("automation/SKILL.md"), "mine").unwrap();
        let checkout = home.join("checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        std::os::unix::fs::symlink(&checkout, skills.join("memory")).unwrap();
        assert!(install(&home, &shipped).is_empty());
        assert_eq!(read(&home, "automation"), "mine");
        assert_eq!(std::fs::read_link(skills.join("memory")).unwrap(), checkout);
        std::fs::remove_dir_all(home.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_skill_no_longer_shipped_loses_only_the_apps_link() {
        let (home, shipped) = setup("dropped", &["automation", "old"]);
        install(&home, &shipped);
        let skills = home.join(".agents/skills");
        // Yours, dangling too, but not into an app.
        std::os::unix::fs::symlink(home.join("gone"), skills.join("mine")).unwrap();
        std::fs::remove_dir_all(shipped.join("old")).unwrap();
        assert!(install(&home, &shipped).is_empty());
        assert!(std::fs::symlink_metadata(skills.join("old")).is_err());
        assert!(std::fs::symlink_metadata(skills.join("mine")).is_ok());
        assert_eq!(read(&home, "automation"), "automation");
        std::fs::remove_dir_all(home.parent().unwrap()).unwrap();
    }
}
