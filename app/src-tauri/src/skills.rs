//! Skills the app ships. An agent reads only skills that are files in its
//! folder's `.agents/skills` or in `~/.agents/skills`, so the app puts each of
//! its own in `~/.agents/skills/NAME/`, where every agent created afterwards
//! finds it. Those files are yours to edit or remove: the app keeps what it
//! last wrote there in `~/.agent/skills/NAME/`, and replaces a skill's files
//! with newer ones only while every one of them still matches. A folder's own
//! skill of the same name wins over it. What the app stops shipping goes on
//! the same terms.
use std::path::Path;

type Files = &'static [(&'static str, &'static str)];

pub const BUILT_IN: [(&str, Files); 2] = [
    (
        "automation",
        &[("SKILL.md", include_str!("../../skills/automation/SKILL.md"))],
    ),
    (
        "workflow",
        &[
            ("SKILL.md", include_str!("../../skills/workflow/SKILL.md")),
            (
                "workflow.py",
                include_str!("../../skills/workflow/workflow.py"),
            ),
        ],
    ),
];

/// Install or update every shipped skill under `home`, and take away what
/// the app wrote of one it no longer ships; one that fails does not stop
/// the others.
pub fn install(home: &Path) -> Vec<String> {
    let mut errors = Vec::new();
    let mut skills: Vec<(String, Files)> = BUILT_IN
        .iter()
        .map(|(name, files)| (name.to_string(), *files))
        .collect();
    let records = home.join(".agent/skills");
    match recorded(&records) {
        Ok(names) => skills.extend(
            names
                .into_iter()
                .filter(|name| BUILT_IN.iter().all(|(shipped, _)| shipped != name))
                .filter(|name| records.join(name).is_dir())
                .map(|name| (name, &[][..])),
        ),
        Err(error) => errors.push(error),
    }
    errors.extend(
        skills
            .iter()
            .filter_map(|(name, files)| install_one(home, name, files).err()),
    );
    errors
}

/// The names in a record folder, its temporaries aside; none when it is
/// absent.
fn recorded(folder: &Path) -> Result<Vec<String>, String> {
    let entries = match std::fs::read_dir(folder) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        entries => entries.map_err(|error| format!("{}: {error}", folder.display()))?,
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("{}: {error}", folder.display()))?;
        if let Some(name) = entry
            .file_name()
            .to_str()
            .filter(|name| !name.starts_with('.'))
        {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

/// More than any skill the app ships; a longer file is not one of its copies.
const MAX: u64 = 256 * 1024;

// Nothing at the path is None. Anything else must open, without waiting,
// as a regular file: a dangling link or a pipe is an error, so it is left
// alone and never holds up the window. At most MAX + 1 bytes are read, so
// a huge file costs no more than a mismatch.
fn read(path: &Path) -> Result<Option<Vec<u8>>, String> {
    use std::{io::Read, os::unix::fs::OpenOptionsExt};
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{}: {error}", path.display())),
        Ok(_) => {}
    }
    let mut bytes = Vec::new();
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .and_then(|file| {
            if !file.metadata()?.is_file() {
                return Err(std::io::Error::other("not a regular file"));
            }
            file.take(MAX + 1).read_to_end(&mut bytes)
        })
        .map(|_| Some(bytes))
        .map_err(|error| format!("{}: {error}", path.display()))
}

fn install_one(home: &Path, name: &str, files: &[(&str, &str)]) -> Result<(), String> {
    let dir = home.join(".agents/skills").join(name);
    let record = home.join(".agent/skills").join(name);
    let mut stale = Vec::new();
    for (file, text) in files {
        let (path, written) = (dir.join(file), record.join(file));
        let have = read(&path)?;
        if have.as_deref() == Some(text.as_bytes()) {
            // Current already; a start that ended before the record still owns it.
            if read(&written)?.as_deref() != Some(text.as_bytes()) {
                crate::schedule::replace(&written, text)?;
            }
            continue;
        }
        // Only a file the app wrote and nobody changed since is the app's: one
        // that was there first, was edited, or was removed is yours, and so is
        // the rest of its skill, which may depend on it.
        if have != read(&written)? {
            return Ok(());
        }
        stale.push((path, written, text));
    }
    // A file the app no longer ships goes too, unless you changed it. One
    // already gone counts as taken, so a start that ended partway finishes.
    let mut dropped = Vec::new();
    for file in recorded(&record)? {
        if files.iter().all(|(shipped, _)| *shipped != file) {
            let (path, written) = (dir.join(&file), record.join(&file));
            let have = read(&path)?;
            if have.is_some() && have != read(&written)? {
                return Ok(());
            }
            dropped.push((path, written));
        }
    }
    // Mine first, then the record, so a crash between them is fixed above.
    for (path, written, text) in stale {
        crate::schedule::replace(&path, text)?;
        crate::schedule::replace(&written, text)?;
    }
    let remove = |path: &Path| match std::fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(format!("{}: {error}", path.display()))
        }
        _ => Ok(()),
    };
    for (path, written) in dropped {
        remove(&path)?;
        remove(&written)?;
    }
    if files.is_empty() {
        // Only if empty: a file you added keeps the folder.
        let _ = std::fs::remove_dir(&dir);
        let _ = std::fs::remove_dir(&record);
    }
    Ok(())
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
        assert_eq!(text, BUILT_IN[0].1[0].1);
        let skills = agent_client::policy::instructions(&home, None)
            .unwrap()
            .skills;
        for (name, files) in BUILT_IN {
            let path = home.join(".agents/skills").join(name).join("SKILL.md");
            assert!(skills.iter().any(|s| s.name == name && s.path == path));
            for (file, text) in files {
                let dir = home.join(".agents/skills").join(name);
                assert_eq!(&std::fs::read_to_string(dir.join(file)).unwrap(), text);
            }
        }
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn newer_text_replaces_the_apps_copy_and_never_yours() {
        let home = home("update");
        install_one(&home, "x", &[("SKILL.md", "old")]).unwrap();
        let path = home.join(".agents/skills/x/SKILL.md");
        install_one(&home, "x", &[("SKILL.md", "new")]).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");

        std::fs::write(&path, "mine").unwrap();
        install_one(&home, "x", &[("SKILL.md", "newer")]).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "mine");

        std::fs::remove_file(&path).unwrap();
        install_one(&home, "x", &[("SKILL.md", "newest")]).unwrap();
        assert!(!path.exists(), "a skill you removed stays removed");
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_skill_that_was_there_first_is_yours() {
        let home = home("first");
        let path = home.join(".agents/skills/x/SKILL.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "from another harness").unwrap();
        install_one(&home, "x", &[("SKILL.md", "app")]).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "from another harness"
        );
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_skill_with_one_file_of_yours_is_all_yours() {
        let home = home("files");
        let dir = home.join(".agents/skills/x");
        let read = |file: &str| std::fs::read_to_string(dir.join(file)).unwrap();
        install_one(&home, "x", &[("SKILL.md", "old"), ("run.py", "old")]).unwrap();
        install_one(&home, "x", &[("SKILL.md", "new"), ("run.py", "new")]).unwrap();
        assert_eq!(
            (read("SKILL.md"), read("run.py")),
            ("new".into(), "new".into())
        );

        // A file a newer version adds is written with the rest.
        let three = [("SKILL.md", "new"), ("run.py", "new"), ("lib.py", "new")];
        install_one(&home, "x", &three).unwrap();
        assert_eq!(read("lib.py"), "new");

        std::fs::write(dir.join("run.py"), "mine").unwrap();
        install_one(&home, "x", &[("SKILL.md", "newer"), ("run.py", "newer")]).unwrap();
        assert_eq!(
            (read("SKILL.md"), read("run.py")),
            ("new".into(), "mine".into())
        );
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_huge_file_is_read_no_further_than_needed_and_stays() {
        let home = home("huge");
        install_one(&home, "x", &[("SKILL.md", "app")]).unwrap();
        let path = home.join(".agents/skills/x/SKILL.md");
        let huge = vec![b'a'; MAX as usize * 4];
        std::fs::write(&path, &huge).unwrap();
        install_one(&home, "x", &[("SKILL.md", "newer")]).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), huge.len() as u64);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_special_file_is_left_alone_without_being_opened() {
        let home = home("fifo");
        let path = home.join(".agents/skills/x/SKILL.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: mkfifo reads the NUL-terminated path it is given.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
        let error = install_one(&home, "x", &[("SKILL.md", "app")]).unwrap_err();
        assert!(error.contains("not a regular file"), "{error}");
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_dangling_link_of_yours_stays() {
        let home = home("dangling");
        let path = home.join(".agents/skills/x/SKILL.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(home.join("gone"), &path).unwrap();
        assert!(install_one(&home, "x", &[("SKILL.md", "app")]).is_err());
        assert!(std::fs::symlink_metadata(&path).unwrap().is_symlink());
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn what_the_app_stops_shipping_goes_unless_you_changed_it() {
        let home = home("dropped");
        let dir = home.join(".agents/skills/x");
        install_one(&home, "x", &[("SKILL.md", "v1"), ("run.py", "v1")]).unwrap();
        install_one(&home, "x", &[("SKILL.md", "v2")]).unwrap();
        assert!(!dir.join("run.py").exists());
        assert!(!home.join(".agent/skills/x/run.py").exists());

        // A skill the app no longer ships at all; `install` finds it by its record.
        install(&home);
        assert!(!dir.exists() && !home.join(".agent/skills/x").exists());

        install_one(&home, "y", &[("SKILL.md", "v1"), ("run.py", "v1")]).unwrap();
        let mine = home.join(".agents/skills/y/run.py");
        std::fs::write(&mine, "mine").unwrap();
        install(&home);
        assert_eq!(std::fs::read_to_string(&mine).unwrap(), "mine");
        assert!(home.join(".agents/skills/y/SKILL.md").exists());
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn every_shipped_skill_fits_the_read_bound() {
        let mut files = BUILT_IN.iter().flat_map(|(_, files)| files.iter());
        assert!(files.all(|(_, text)| (text.len() as u64) <= MAX));
    }

    #[test]
    fn a_start_that_ended_before_its_record_still_updates_later() {
        let home = home("crash");
        install_one(&home, "x", &[("SKILL.md", "old")]).unwrap();
        let path = home.join(".agents/skills/x/SKILL.md");
        // The file was replaced, then the app stopped before the record.
        std::fs::write(&path, "new").unwrap();
        install_one(&home, "x", &[("SKILL.md", "new")]).unwrap();
        install_one(&home, "x", &[("SKILL.md", "newer")]).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "newer");
        std::fs::remove_dir_all(home).unwrap();
    }
}
