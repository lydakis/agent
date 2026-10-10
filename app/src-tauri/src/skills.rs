//! Skills the app ships. An agent reads only skills in its folder's
//! `.agents/skills` or in `~/.agents/skills`, so the app links each skill in
//! its bundle's `Contents/Resources/skills/NAME` from `~/.agents/skills/NAME`.
//! A link is never stale: updating the app updates what it points at, with
//! nothing copied or recorded. A folder, file or link of yours at that name
//! is left alone, and a folder's own skill of the same name wins over it.
//! The app links at every start; the Homebrew cask runs `--link-skills`
//! after an install or upgrade and `--unlink-skills` before an uninstall.
use std::path::{Path, PathBuf};

pub const LINK_FLAG: &str = "--link-skills";
pub const UNLINK_FLAG: &str = "--unlink-skills";

/// Where a bundled app keeps its skills, beside `Contents/MacOS/BINARY`.
pub fn bundled(exe: &Path) -> Option<PathBuf> {
    let skills = exe.parent()?.parent()?.join("Resources/skills");
    skills.is_dir().then_some(skills)
}

/// `--link-skills` or `--unlink-skills`: this bundle's links, then exit.
pub fn cli(link: bool) -> i32 {
    let Some(home) = std::env::var_os("HOME") else {
        eprintln!("agent-app: HOME is not set");
        return 1;
    };
    let Some(shipped) = std::env::current_exe().ok().and_then(|exe| bundled(&exe)) else {
        eprintln!("agent-app: no skills beside this executable");
        return 1;
    };
    let home = Path::new(&home);
    let errors = if link {
        install(home, &shipped)
    } else {
        uninstall(home, &shipped)
    };
    for error in &errors {
        eprintln!("agent-app: {error}");
    }
    i32::from(!errors.is_empty())
}

/// A link the app made: one into `NAME.app/Contents/Resources/skills` of a
/// copy of this app, known by its `Contents/MacOS/agent-app`, or of an app
/// since removed (the link leads nowhere, and the skill index refuses it).
/// Another app's skills folder, linked by you, is yours.
fn ours(target: &Path) -> bool {
    let mut up = target.ancestors().skip(1);
    let (Some(skills), Some(resources), Some(contents), Some(app)) =
        (up.next(), up.next(), up.next(), up.next())
    else {
        return false;
    };
    let named = |path: &Path, name: &str| path.file_name().is_some_and(|file| file == name);
    named(skills, "skills")
        && named(resources, "Resources")
        && named(contents, "Contents")
        && app.extension().is_some_and(|extension| extension == "app")
        && (contents.join("MacOS/agent-app").is_file() || std::fs::symlink_metadata(app).is_err())
}

/// Link every skill in `shipped` from `home`'s `~/.agents/skills`, point the
/// app's links that lead elsewhere (an app since moved, or another copy) here,
/// and remove the app's links to skills this copy does not ship. One that
/// fails does not stop the others.
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
            Ok(current) => current != target && ours(&dir.join(current)),
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
    // A skill this copy does not ship keeps no link of the app's, whether it
    // leads nowhere or into another copy. The index reads no more of the
    // folder than its own bound, nor does this.
    remove(
        &dir,
        |name, target| !names.iter().any(|shipped| shipped == name) && ours(target),
        &mut fail,
    );
    errors
}

/// Remove the links into `shipped` that `install` made, and nothing else.
pub fn uninstall(home: &Path, shipped: &Path) -> Vec<String> {
    let mut errors = Vec::new();
    let mut fail = |path: &Path, error: std::io::Error| {
        errors.push(format!("{}: {error}", path.display()));
    };
    let dir = home.join(".agents/skills");
    remove(&dir, |name, target| target == shipped.join(name), &mut fail);
    errors
}

/// Remove each link in `dir` whose name and target `doomed` picks.
fn remove(
    dir: &Path,
    doomed: impl Fn(&std::ffi::OsStr, &Path) -> bool,
    fail: &mut impl FnMut(&Path, std::io::Error),
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.take(4096).flatten() {
        let link = entry.path();
        if let Ok(target) = std::fs::read_link(&link)
            && doomed(&entry.file_name(), &dir.join(target))
            && let Err(error) = std::fs::remove_file(&link)
        {
            fail(&link, error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A home and a bundle `Agent.app` shipping `names`.
    fn setup(tag: &str, names: &[&str]) -> (PathBuf, PathBuf) {
        let root =
            std::env::temp_dir().join(format!("agent-app-skills-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let shipped = bundle(&root.join("Agent.app"), names, true);
        (root.join("home"), shipped)
    }

    /// An app at `app` shipping `names`; `agent` makes it a copy of this one.
    fn bundle(app: &Path, names: &[&str], agent: bool) -> PathBuf {
        let shipped = app.join("Contents/Resources/skills");
        for name in names {
            std::fs::create_dir_all(shipped.join(name)).unwrap();
            std::fs::write(shipped.join(name).join("SKILL.md"), name).unwrap();
        }
        if agent {
            std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
            std::fs::write(app.join("Contents/MacOS/agent-app"), "").unwrap();
        }
        shipped
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
    #[test]
    fn another_apps_skill_you_linked_stays() {
        let (home, shipped) = setup("other", &["automation", "memory"]);
        let root = home.parent().unwrap().to_path_buf();
        let other = bundle(&root.join("Other.app"), &["automation", "notes"], false);
        let skills = home.join(".agents/skills");
        std::fs::create_dir_all(&skills).unwrap();
        for name in ["automation", "notes"] {
            std::os::unix::fs::symlink(other.join(name), skills.join(name)).unwrap();
        }
        assert!(install(&home, &shipped).is_empty());
        assert_eq!(read(&home, "automation"), "automation");
        assert_eq!(
            std::fs::read_link(skills.join("automation")).unwrap(),
            other.join("automation")
        );
        assert_eq!(
            std::fs::read_link(skills.join("notes")).unwrap(),
            other.join("notes")
        );
        assert_eq!(read(&home, "memory"), "memory");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_skill_another_copy_still_ships_loses_its_link() {
        let (home, shipped) = setup("copies", &["automation"]);
        let root = home.parent().unwrap().to_path_buf();
        let older = bundle(
            &root.join("Downloads/Agent.app"),
            &["automation", "old"],
            true,
        );
        install(&home, &older);
        assert!(install(&home, &shipped).is_empty());
        let skills = home.join(".agents/skills");
        assert!(std::fs::symlink_metadata(skills.join("old")).is_err());
        assert_eq!(
            std::fs::read_link(skills.join("automation")).unwrap(),
            shipped.join("automation")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn uninstall_removes_only_this_copys_links() {
        let (home, shipped) = setup("uninstall", &["automation", "memory"]);
        let root = home.parent().unwrap().to_path_buf();
        install(&home, &shipped);
        let skills = home.join(".agents/skills");
        std::fs::remove_file(skills.join("memory")).unwrap();
        std::fs::create_dir_all(skills.join("memory")).unwrap();
        let other = bundle(&root.join("Other.app"), &["notes"], false);
        std::os::unix::fs::symlink(other.join("notes"), skills.join("notes")).unwrap();
        assert!(uninstall(&home, &shipped).is_empty());
        assert!(std::fs::symlink_metadata(skills.join("automation")).is_err());
        assert!(skills.join("memory").is_dir());
        assert!(std::fs::symlink_metadata(skills.join("notes")).is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }
}
