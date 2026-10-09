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

pub const BUILT_IN: [(&str, Files); 1] = [(
    "automation",
    &[("SKILL.md", include_str!("../../skills/automation/SKILL.md"))],
)];

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
    let mut budget = ENTRIES;
    match recorded(&records, &mut budget) {
        Ok(names) => skills.extend(
            names
                .into_iter()
                .filter(|name| BUILT_IN.iter().all(|(shipped, _)| shipped != name))
                .filter(|name| records.join(name).is_dir())
                .map(|name| (name, &[][..])),
        ),
        Err(error) => errors.push(error),
    }
    for (name, files) in &skills {
        if let Err(error) = install_one(home, name, files, &mut budget) {
            errors.push(error);
        }
        if budget == 0 {
            break;
        }
    }
    errors
}

/// More entries than all the app's records hold together. Past it, what is
/// left is not the app's, so a start walks no further.
const ENTRIES: usize = 1024;

/// The names in a record folder, its temporaries aside; none when it is
/// absent. Every entry, listed or not, spends one of `budget`; running out
/// is an error.
fn recorded(folder: &Path, budget: &mut usize) -> Result<Vec<String>, String> {
    let entries = match std::fs::read_dir(folder) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        entries => entries.map_err(|error| format!("{}: {error}", folder.display()))?,
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("{}: {error}", folder.display()))?;
        if *budget == 0 {
            return Err(format!("{}: more than {ENTRIES} entries", folder.display()));
        }
        *budget -= 1;
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

/// Sync every folder from `path`'s up to `home`, so folders a first write
/// created are on disk with it.
fn settle(path: &Path, home: &Path) -> Result<(), String> {
    for folder in path.ancestors().skip(1).take_while(|f| f.starts_with(home)) {
        std::fs::File::open(folder)
            .and_then(|folder| folder.sync_all())
            .map_err(|error| format!("{}: {error}", folder.display()))?;
    }
    Ok(())
}

/// Every file under a skill's record, as a path relative to it.
fn recorded_files(record: &Path, budget: &mut usize) -> Result<Vec<String>, String> {
    let (mut files, mut folders) = (Vec::new(), vec![String::new()]);
    while let Some(folder) = folders.pop() {
        for name in recorded(&record.join(&folder), budget)? {
            let file = match folder.as_str() {
                "" => name,
                folder => format!("{folder}/{name}"),
            };
            let path = record.join(&file);
            match std::fs::symlink_metadata(&path) {
                Ok(kind) if kind.is_dir() => folders.push(file),
                Ok(_) => files.push(file),
                Err(error) => return Err(format!("{}: {error}", path.display())),
            }
        }
    }
    Ok(files)
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

/// `budget` is shared by every record a start walks, so the whole walk is
/// bounded, not each record.
fn install_one(
    home: &Path,
    name: &str,
    files: &[(&str, &str)],
    budget: &mut usize,
) -> Result<(), String> {
    let dir = home.join(".agents/skills").join(name);
    let record = home.join(".agent/skills").join(name);
    // A file or folder of the skill's that is a link leads somewhere the app
    // did not write, such as a checkout of yours, so the skill is yours.
    let linked = |file: &Path, top: &Path| {
        file.ancestors()
            .take_while(|folder| folder.starts_with(top))
            .any(|folder| std::fs::symlink_metadata(folder).is_ok_and(|m| m.is_symlink()))
    };
    let (mut stale, mut repair) = (Vec::new(), Vec::new());
    for (file, text) in files {
        let (path, written) = (dir.join(file), record.join(file));
        if linked(&path, &dir) || linked(&written, &record) {
            return Ok(());
        }
        let have = read(&path)?;
        if have.as_deref() == Some(text.as_bytes()) {
            // Current already; a start that ended before the record still
            // owns it, once the rest of the skill proves to be the app's.
            if read(&written)?.as_deref() != Some(text.as_bytes()) {
                repair.push((written, text));
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
    // A file the app no longer ships goes too, unless you changed or removed
    // it, which makes the skill yours like any other file.
    let mut dropped = Vec::new();
    for file in recorded_files(&record, budget)? {
        if files.iter().all(|(shipped, _)| *shipped != file) {
            let (path, written) = (dir.join(&file), record.join(&file));
            if linked(&path, &dir) || linked(&written, &record) {
                return Ok(());
            }
            let have = read(&path)?;
            if have != read(&written)? {
                return Ok(());
            }
            dropped.push((path, written));
        }
    }
    for (written, text) in repair {
        crate::schedule::replace(&written, text)?;
        settle(&written, home)?;
    }
    // Mine first, then the record, so a crash between them is fixed above.
    for (path, written, text) in stale {
        crate::schedule::replace(&path, text)?;
        settle(&path, home)?;
        crate::schedule::replace(&written, text)?;
        settle(&written, home)?;
    }
    // Each removal is on disk before its record goes, so a power loss never
    // leaves a file the app no longer knows it wrote. SKILL.md goes first: a
    // start that ends partway leaves the rest yours, and unindexed.
    dropped.sort_by_key(|(path, _)| !path.ends_with("SKILL.md"));
    for (path, written) in dropped {
        crate::schedule::forget(&path)?;
        crate::schedule::forget(&written)?;
        for (file, top) in [(&path, &dir), (&written, &record)] {
            for folder in file.ancestors().skip(1).take_while(|f| f.starts_with(top)) {
                let _ = std::fs::remove_dir(folder);
            }
        }
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

    fn one(home: &Path, name: &str, files: &[(&str, &str)]) -> Result<(), String> {
        install_one(home, name, files, &mut { ENTRIES })
    }

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
        one(&home, "x", &[("SKILL.md", "old")]).unwrap();
        let path = home.join(".agents/skills/x/SKILL.md");
        one(&home, "x", &[("SKILL.md", "new")]).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");

        std::fs::write(&path, "mine").unwrap();
        one(&home, "x", &[("SKILL.md", "newer")]).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "mine");

        std::fs::remove_file(&path).unwrap();
        one(&home, "x", &[("SKILL.md", "newest")]).unwrap();
        assert!(!path.exists(), "a skill you removed stays removed");
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_skill_that_was_there_first_is_yours() {
        let home = home("first");
        let path = home.join(".agents/skills/x/SKILL.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "from another harness").unwrap();
        one(&home, "x", &[("SKILL.md", "app")]).unwrap();
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
        one(&home, "x", &[("SKILL.md", "old"), ("run.py", "old")]).unwrap();
        one(&home, "x", &[("SKILL.md", "new"), ("run.py", "new")]).unwrap();
        assert_eq!(
            (read("SKILL.md"), read("run.py")),
            ("new".into(), "new".into())
        );

        // A file a newer version adds is written with the rest.
        let three = [("SKILL.md", "new"), ("run.py", "new"), ("lib.py", "new")];
        one(&home, "x", &three).unwrap();
        assert_eq!(read("lib.py"), "new");

        std::fs::write(dir.join("run.py"), "mine").unwrap();
        one(&home, "x", &[("SKILL.md", "newer"), ("run.py", "newer")]).unwrap();
        assert_eq!(
            (read("SKILL.md"), read("run.py")),
            ("new".into(), "mine".into())
        );
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_huge_file_is_read_no_further_than_needed_and_stays() {
        let home = home("huge");
        one(&home, "x", &[("SKILL.md", "app")]).unwrap();
        let path = home.join(".agents/skills/x/SKILL.md");
        let huge = vec![b'a'; MAX as usize * 4];
        std::fs::write(&path, &huge).unwrap();
        one(&home, "x", &[("SKILL.md", "newer")]).unwrap();
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
        let error = one(&home, "x", &[("SKILL.md", "app")]).unwrap_err();
        assert!(error.contains("not a regular file"), "{error}");
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_dangling_link_of_yours_stays() {
        let home = home("dangling");
        let path = home.join(".agents/skills/x/SKILL.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(home.join("gone"), &path).unwrap();
        one(&home, "x", &[("SKILL.md", "app")]).unwrap();
        assert!(std::fs::symlink_metadata(&path).unwrap().is_symlink());
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn what_the_app_stops_shipping_goes_unless_you_changed_it() {
        let home = home("dropped");
        let dir = home.join(".agents/skills/x");
        one(&home, "x", &[("SKILL.md", "v1"), ("run.py", "v1")]).unwrap();
        one(&home, "x", &[("SKILL.md", "v2")]).unwrap();
        assert!(!dir.join("run.py").exists());
        assert!(!home.join(".agent/skills/x/run.py").exists());

        // A file in a folder of its own updates, and goes with its folder.
        let nested = [("SKILL.md", "v2"), ("scripts/run.py", "v1")];
        one(&home, "x", &nested).unwrap();
        one(&home, "x", &[("SKILL.md", "v2"), ("scripts/run.py", "v2")]).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("scripts/run.py")).unwrap(),
            "v2"
        );
        one(&home, "x", &[("SKILL.md", "v2")]).unwrap();
        assert!(!dir.join("scripts").exists());
        assert!(!home.join(".agent/skills/x/scripts").exists());

        // A skill the app no longer ships at all; `install` finds it by its record.
        install(&home);
        assert!(!dir.exists() && !home.join(".agent/skills/x").exists());

        one(&home, "y", &[("SKILL.md", "v1"), ("run.py", "v1")]).unwrap();
        let mine = home.join(".agents/skills/y/run.py");
        std::fs::write(&mine, "mine").unwrap();
        install(&home);
        assert_eq!(std::fs::read_to_string(&mine).unwrap(), "mine");
        assert!(home.join(".agents/skills/y/SKILL.md").exists());

        // One you removed makes the rest yours too.
        one(&home, "z", &[("SKILL.md", "v1"), ("run.py", "v1")]).unwrap();
        std::fs::remove_file(home.join(".agents/skills/z/run.py")).unwrap();
        install(&home);
        assert!(home.join(".agents/skills/z/SKILL.md").exists());
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_record_too_large_to_be_the_apps_is_left_alone() {
        let home = home("entries");
        let record = home.join(".agent/skills/x");
        std::fs::create_dir_all(&record).unwrap();
        for n in 0..=ENTRIES {
            // Hidden or not, every entry counts.
            std::fs::write(record.join(format!(".{n}")), "").unwrap();
        }
        let error = one(&home, "x", &[]).unwrap_err();
        assert!(error.contains("more than"), "{error}");
        assert_eq!(std::fs::read_dir(&record).unwrap().count(), ENTRIES + 1);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn one_bound_covers_every_record_a_start_walks() {
        let home = home("aggregate");
        for skill in 0..4 {
            let record = home.join(".agent/skills").join(format!("s{skill}"));
            std::fs::create_dir_all(&record).unwrap();
            for n in 0..ENTRIES / 3 {
                std::fs::write(record.join(format!(".{n}")), "").unwrap();
            }
        }
        let errors = install(&home);
        assert!(errors.iter().any(|e| e.contains("more than")), "{errors:?}");
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_skill_folder_that_is_a_link_is_yours() {
        let home = home("linked");
        one(&home, "x", &[("SKILL.md", "old")]).unwrap();
        let (dir, checkout) = (home.join(".agents/skills/x"), home.join("checkout"));
        std::fs::rename(&dir, &checkout).unwrap();
        std::os::unix::fs::symlink(&checkout, &dir).unwrap();
        one(&home, "x", &[("SKILL.md", "new")]).unwrap();
        one(&home, "x", &[]).unwrap();
        let text = std::fs::read_to_string(checkout.join("SKILL.md")).unwrap();
        assert_eq!(text, "old");
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_file_you_linked_stays_a_link() {
        let home = home("file-link");
        one(&home, "x", &[("SKILL.md", "old")]).unwrap();
        let path = home.join(".agents/skills/x/SKILL.md");
        let mine = home.join("mine.md");
        std::fs::write(&mine, "old").unwrap();
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&mine, &path).unwrap();
        one(&home, "x", &[("SKILL.md", "new")]).unwrap();
        assert!(std::fs::symlink_metadata(&path).unwrap().is_symlink());
        assert_eq!(std::fs::read_to_string(&mine).unwrap(), "old");
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_temporary_left_as_a_link_is_never_written_through() {
        let home = home("temp-link");
        one(&home, "x", &[("SKILL.md", "old")]).unwrap();
        let dir = home.join(".agents/skills/x");
        let victim = home.join("victim");
        std::fs::write(&victim, "keep").unwrap();
        let temporary = dir.join(format!(".SKILL.md.{}", std::process::id()));
        std::os::unix::fs::symlink(&victim, &temporary).unwrap();
        one(&home, "x", &[("SKILL.md", "new")]).unwrap();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
        assert_eq!(
            std::fs::read_to_string(dir.join("SKILL.md")).unwrap(),
            "new"
        );
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_record_is_repaired_only_for_a_skill_that_is_still_the_apps() {
        let home = home("repair");
        let files = [("SKILL.md", "old"), ("run.py", "old")];
        one(&home, "x", &files).unwrap();
        let dir = home.join(".agents/skills/x");
        // An update replaced SKILL.md and stopped; then you edited run.py.
        std::fs::write(dir.join("SKILL.md"), "new").unwrap();
        std::fs::write(dir.join("run.py"), "mine").unwrap();
        one(&home, "x", &[("SKILL.md", "new"), ("run.py", "new")]).unwrap();
        let record = home.join(".agent/skills/x/SKILL.md");
        assert_eq!(std::fs::read_to_string(record).unwrap(), "old");
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
        one(&home, "x", &[("SKILL.md", "old")]).unwrap();
        let path = home.join(".agents/skills/x/SKILL.md");
        // The file was replaced, then the app stopped before the record.
        std::fs::write(&path, "new").unwrap();
        one(&home, "x", &[("SKILL.md", "new")]).unwrap();
        one(&home, "x", &[("SKILL.md", "newer")]).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "newer");
        std::fs::remove_dir_all(home).unwrap();
    }
}
