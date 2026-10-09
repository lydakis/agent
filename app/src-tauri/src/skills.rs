//! Skills the app ships. An agent reads only skills that are files in its
//! folder's `.agents/skills` or in `~/.agents/skills`, so the app puts each of
//! its own in `~/.agents/skills/NAME/`, where every agent created afterwards
//! finds it. Those files are yours to edit or remove: the app keeps what it
//! last wrote there in `~/.agent/skills/NAME/`, and replaces a skill's files
//! with newer ones only while every one of them still matches. A folder's own
//! skill of the same name wins over it.
use std::path::Path;

type Files = &'static [(&'static str, &'static str)];

pub const BUILT_IN: [(&str, Files); 1] = [(
    "automation",
    &[("SKILL.md", include_str!("../../skills/automation/SKILL.md"))],
)];

/// Install or update every shipped skill under `home`; one that fails does
/// not stop the others.
pub fn install(home: &Path) -> Vec<String> {
    BUILT_IN
        .iter()
        .filter_map(|(name, files)| install_one(home, name, files).err())
        .collect()
}

/// More than any skill the app ships; a longer file is not one of its copies.
const MAX: u64 = 256 * 1024;

fn install_one(home: &Path, name: &str, files: &[(&str, &str)]) -> Result<(), String> {
    let dir = home.join(".agents/skills").join(name);
    let record = home.join(".agent/skills").join(name);
    // A regular file only, at most MAX + 1 bytes of it, so a huge file costs
    // no more than a mismatch and a pipe never holds up the window.
    let read = |path: &Path| {
        use std::io::Read;
        let opened = std::fs::metadata(path).and_then(|m| {
            if m.is_file() {
                std::fs::File::open(path)
            } else {
                Err(std::io::Error::other("not a regular file"))
            }
        });
        let mut bytes = Vec::new();
        match opened.and_then(|f| f.take(MAX + 1).read_to_end(&mut bytes)) {
            Ok(_) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("{}: {error}", path.display())),
        }
    };
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
    // Mine first, then the record, so a crash between them is fixed above.
    for (path, written, text) in stale {
        crate::schedule::replace(&path, text)?;
        crate::schedule::replace(&written, text)?;
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
