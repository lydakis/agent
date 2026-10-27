//! Sonny's skills, written into the person's `~/.agents/skills`, where
//! every agent finds them, and the scripts in `~/.agent` they run.
use std::path::{Path, PathBuf};

/// The `errand` skill, for every agent, when Errand is installed.
pub fn install_errand() {
    let on_path = std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join("errand").is_file()));
    if on_path {
        install("errand", include_str!("../skills/errand/SKILL.md"));
    }
}

/// What every agent of Sonny's leans on: the memory, plan and automation
/// skills, the nightly cleanup's, and `~/.agent/memory`, `~/.agent/project`
/// and `~/.agent/trigger`, which run this Sonny. Written at each start, so
/// they lead to this copy.
pub fn install_own() {
    install("memory", include_str!("../skills/memory/SKILL.md"));
    install(
        "memory-cleanup",
        include_str!("../skills/memory-cleanup/SKILL.md"),
    );
    install_files(
        "plan",
        &[
            ("SKILL.md", include_str!("../skills/plan/SKILL.md"), 0o644),
            ("plan", include_str!("../skills/plan/plan"), 0o755),
        ],
    );
    // A job reaches the daemon the way every agent Sonny runs does.
    let automation = include_str!("../skills/automation/SKILL.md");
    install("automation", &agent_client::policy::with_cli(automation));
    let (Some(home), Some(sonny)) = (std::env::var_os("HOME"), sonny_path()) else {
        return;
    };
    let state = PathBuf::from(home).join(".agent");
    for (name, usage) in [
        ("memory", agent_client::memory::usage()),
        ("project", crate::project::usage()),
        (crate::trigger::VERB, crate::trigger::USAGE),
    ] {
        if let Err(error) = write_runner(&state.join(name), usage, &sonny, name) {
            eprintln!("sonny: {error}");
        }
    }
}

/// A skill of Sonny's in the person's skills, written as a normal skill:
/// rewritten when Sonny's text changed, never over the person's edits.
pub fn install(name: &str, text: &str) -> Option<PathBuf> {
    install_files(name, &[("SKILL.md", text, 0o644)])
}

/// A skill's files, each with its mode.
fn install_files(name: &str, files: &[(&str, &str, u32)]) -> Option<PathBuf> {
    install_in(&PathBuf::from(std::env::var_os("HOME")?), name, files)
}

/// Where Sonny notes what it last wrote of each skill: a line of
/// `FINGERPRINT FILE` for each file.
fn records(home: &Path) -> PathBuf {
    home.join(".agent/sonny/skills")
}

/// The fingerprints Sonny last wrote of `name`'s files, if it noted any.
fn record(home: &Path, name: &str) -> Option<Vec<(String, String)>> {
    let text = std::fs::read_to_string(records(home).join(name)).ok()?;
    Some(
        text.lines()
            .filter_map(|line| {
                let (hash, file) = line.split_once(' ')?;
                Some((file.to_owned(), hash.to_owned()))
            })
            .collect(),
    )
}

/// Whether the skill at `dir` is Sonny's to change: missing, or as Sonny
/// last wrote it. One Sonny never noted writing is the person's.
fn sonnys(home: &Path, name: &str, dir: &Path) -> bool {
    if !dir.exists() {
        return true;
    }
    match record(home, name) {
        Some(files) => files.iter().all(|(file, hash)| {
            std::fs::read_to_string(dir.join(file))
                .is_ok_and(|now| crate::project::fingerprint(&now) == *hash)
        }),
        None => false,
    }
}

/// `files` as the skill `name` in `home`'s skills, rewritten when one
/// changed and the skill is still Sonny's. A link the app left there,
/// into its bundle, is replaced, never written through; a link of the
/// person's own is left alone.
fn install_in(home: &Path, name: &str, files: &[(&str, &str, u32)]) -> Option<PathBuf> {
    let dir = home.join(".agents/skills").join(name);
    let skill = dir.join("SKILL.md");
    if let Ok(target) = std::fs::read_link(&dir) {
        if !in_app_bundle(&target) {
            return None;
        }
        std::fs::remove_file(&dir).ok()?;
    }
    let same = files.iter().all(|(file, text, _)| {
        std::fs::read_to_string(dir.join(file)).is_ok_and(|now| now == *text)
    });
    let wrote = same || sonnys(home, name, &dir);
    if !same && wrote {
        std::fs::create_dir_all(&dir).ok()?;
        for (file, text, mode) in files {
            agent_client::files::replace_mode(&dir.join(file), text.as_bytes(), *mode).ok()?;
        }
    }
    if wrote {
        let noted: String = files
            .iter()
            .map(|(file, text, _)| format!("{} {file}\n", crate::project::fingerprint(text)))
            .collect();
        let path = records(home).join(name);
        if std::fs::read_to_string(&path).ok().as_deref() != Some(noted.as_str()) {
            let _ = std::fs::create_dir_all(records(home));
            let _ = agent_client::files::replace_mode(&path, noted.as_bytes(), 0o644);
        }
    }
    (!same && wrote).then_some(skill)
}

/// Whether a link leads into `NAME.app/Contents/Resources/skills`, where
/// the app linked the skills it shipped.
fn in_app_bundle(target: &Path) -> bool {
    let mut up = target.ancestors().skip(1);
    let named = |path: Option<&Path>, name: &str| {
        path.and_then(Path::file_name)
            .is_some_and(|file| file == name)
    };
    named(up.next(), "skills")
        && named(up.next(), "Resources")
        && named(up.next(), "Contents")
        && up
            .next()
            .and_then(Path::extension)
            .is_some_and(|extension| extension == "app")
}

/// This Sonny, by the path that outlives an upgrade: Homebrew's `opt`
/// link for a Sonny in its Cellar, which a scheduled fire still finds after
/// the old version is cleaned up.
pub fn sonny_path() -> Option<PathBuf> {
    let me = std::env::current_exe().ok()?;
    Some(opt_path(&std::fs::canonicalize(&me).unwrap_or(me)))
}

/// `PREFIX/Cellar/NAME/VERSION/REST` as `PREFIX/opt/NAME/REST` when that
/// leads to a file; any other path as it is.
fn opt_path(exe: &Path) -> PathBuf {
    let parts: Vec<_> = exe.components().collect();
    let found = parts
        .iter()
        .position(|part| part.as_os_str() == "Cellar")
        .filter(|at| at + 3 < parts.len());
    let Some(at) = found else {
        return exe.to_owned();
    };
    let mut opt: PathBuf = parts[..at].iter().collect();
    opt.push("opt");
    opt.push(parts[at + 1]);
    opt.extend(&parts[at + 3..]);
    if opt.is_file() { opt } else { exe.to_owned() }
}

/// A script at `path` that runs `sonny` with `verb`, its usage in
/// comments: written when it differs, so a start that changes nothing
/// writes nothing.
fn write_runner(path: &Path, usage: &str, sonny: &Path, verb: &str) -> Result<(), String> {
    let quote = |p: &Path| format!("'{}'", p.to_string_lossy().replace('\'', r"'\''"));
    let usage = usage.replace('\n', "\n# ");
    let text = format!(
        "#!/bin/sh\n# {usage}\nexec {} {verb} \"$@\"\n",
        quote(sonny)
    );
    use std::os::unix::fs::PermissionsExt;
    let runnable = std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o777 == 0o755);
    if runnable && std::fs::read_to_string(path).is_ok_and(|have| have == text) {
        return Ok(());
    }
    agent_client::files::replace_mode(path, text.as_bytes(), 0o755)
}

/// Take a skill of Sonny's out of the person's skills, unless the person
/// edited it.
pub fn remove(name: &str) {
    if let Some(home) = std::env::var_os("HOME") {
        remove_in(&PathBuf::from(home), name);
    }
}

fn remove_in(home: &Path, name: &str) {
    let dir = home.join(".agents/skills").join(name);
    if std::fs::read_link(&dir).is_ok() || !sonnys(home, name, &dir) {
        return;
    }
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_file(records(home).join(name));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_runner_runs_this_sonny_with_its_verb() {
        let dir = std::env::temp_dir().join(format!("sonny-runner-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("memory");
        let sonny = Path::new("/opt/it's/sonny");
        write_runner(
            &path,
            "usage: memory show\n       memory rm",
            sonny,
            "memory",
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            "#!/bin/sh\n# usage: memory show\n#        memory rm\nexec '/opt/it'\\''s/sonny' memory \"$@\"\n"
        );
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o755);
        // Its text survived a crash before its mode did.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_runner(
            &path,
            "usage: memory show\n       memory rm",
            sonny,
            "memory",
        )
        .unwrap();
        assert_eq!(mode(&path), 0o755);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_sonny_in_homebrews_cellar_goes_by_its_opt_link() {
        let root = std::env::temp_dir().join(format!("sonny-opt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let cellar = root.join("Cellar/sonny/0.3.2/libexec/sonny");
        std::fs::create_dir_all(cellar.parent().unwrap()).unwrap();
        std::fs::write(&cellar, "").unwrap();
        // Without the link, the path itself.
        assert_eq!(opt_path(&cellar), cellar);
        let opt = root.join("opt/sonny/libexec/sonny");
        std::fs::create_dir_all(opt.parent().unwrap()).unwrap();
        std::fs::write(&opt, "").unwrap();
        assert_eq!(opt_path(&cellar), opt);
        assert_eq!(
            opt_path(Path::new("/usr/local/bin/sonny")),
            Path::new("/usr/local/bin/sonny")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_skill_is_rewritten_until_the_person_edits_it() {
        let home = std::env::temp_dir().join(format!("sonny-skills-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let skill = home.join(".agents/skills/plan/SKILL.md");
        let read = || std::fs::read_to_string(&skill).unwrap();
        let files = |text| [("SKILL.md", text, 0o644)];
        assert_eq!(
            install_in(&home, "plan", &files("one")),
            Some(skill.clone())
        );
        assert_eq!(read(), "one");
        // Sonny's next text replaces its own.
        assert!(install_in(&home, "plan", &files("two")).is_some());
        assert_eq!(read(), "two");
        // The person's edit stays, through Sonny's next text and a remove.
        std::fs::write(&skill, "mine").unwrap();
        assert!(install_in(&home, "plan", &files("three")).is_none());
        assert_eq!(read(), "mine");
        remove_in(&home, "plan");
        assert_eq!(read(), "mine");
        // A skill of the person's own of that name, which Sonny never wrote, too.
        let theirs = home.join(".agents/skills/errand/SKILL.md");
        std::fs::create_dir_all(theirs.parent().unwrap()).unwrap();
        std::fs::write(&theirs, "theirs").unwrap();
        assert!(install_in(&home, "errand", &files("sonny's")).is_none());
        assert_eq!(std::fs::read_to_string(&theirs).unwrap(), "theirs");
        remove_in(&home, "errand");
        assert_eq!(std::fs::read_to_string(&theirs).unwrap(), "theirs");
        // Once it is gone, Sonny's is written, and an unedited one goes on a remove.
        std::fs::remove_dir_all(theirs.parent().unwrap()).unwrap();
        assert!(install_in(&home, "errand", &files("sonny's")).is_some());
        assert_eq!(std::fs::read_to_string(&theirs).unwrap(), "sonny's");
        remove_in(&home, "errand");
        assert!(!theirs.exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn only_the_apps_links_are_replaced() {
        assert!(in_app_bundle(Path::new(
            "/Applications/Agent.app/Contents/Resources/skills/memory"
        )));
        assert!(!in_app_bundle(Path::new("/Users/g/skills/memory")));
        assert!(!in_app_bundle(Path::new("memory")));
    }
}
