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

/// What every agent of Sonny's leans on: the memory and plan skills, the
/// nightly cleanup's, and `~/.agent/memory` and `~/.agent/project`, which
/// run this Sonny. Written at each start, so they lead to this copy.
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
    let (Some(home), Ok(sonny)) = (std::env::var_os("HOME"), std::env::current_exe()) else {
        return;
    };
    let state = PathBuf::from(home).join(".agent");
    for (name, usage) in [
        ("memory", agent_client::memory::usage()),
        ("project", crate::project::usage()),
    ] {
        if let Err(error) = write_runner(&state.join(name), usage, &sonny, name) {
            eprintln!("sonny: {error}");
        }
    }
}

/// A skill of Sonny's in the person's skills, rewritten when it changed.
pub fn install(name: &str, text: &str) -> Option<PathBuf> {
    install_files(name, &[("SKILL.md", text, 0o644)])
}

/// A skill's files, each with its mode, rewritten when one changed. A
/// link the app left there, into its bundle, is replaced, never written
/// through; a link of the person's own is left alone.
fn install_files(name: &str, files: &[(&str, &str, u32)]) -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("HOME")?)
        .join(".agents/skills")
        .join(name);
    let skill = dir.join("SKILL.md");
    let same = files.iter().all(|(file, text, _)| {
        std::fs::read_to_string(dir.join(file)).is_ok_and(|now| now == *text)
    });
    if let Ok(target) = std::fs::read_link(&dir) {
        if !in_app_bundle(&target) {
            return None;
        }
        std::fs::remove_file(&dir).ok()?;
    } else if same {
        return None;
    }
    std::fs::create_dir_all(&dir).ok()?;
    for (file, text, mode) in files {
        agent_client::files::replace_mode(&dir.join(file), text.as_bytes(), *mode).ok()?;
    }
    Some(skill)
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

/// Take a skill of Sonny's out of the person's skills.
pub fn remove(name: &str) {
    if let Some(home) = std::env::var_os("HOME") {
        let _ = std::fs::remove_dir_all(PathBuf::from(home).join(".agents/skills").join(name));
    }
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
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        let _ = std::fs::remove_dir_all(&dir);
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
