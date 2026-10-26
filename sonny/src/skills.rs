//! Sonny's skills, written into the person's `~/.agents/skills`, where
//! every agent finds them.
use std::path::PathBuf;

/// The `errand` skill, for every agent, when Errand is installed.
pub fn install_errand() {
    let on_path = std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join("errand").is_file()));
    if on_path {
        install("errand", include_str!("../skills/errand/SKILL.md"));
    }
}

/// A skill of Sonny's in the person's skills, rewritten when it changed.
pub fn install(name: &str, text: &str) -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("HOME")?)
        .join(".agents/skills")
        .join(name);
    let path = dir.join("SKILL.md");
    if std::fs::read_to_string(&path).is_ok_and(|now| now == text) {
        return None;
    }
    std::fs::create_dir_all(&dir).ok()?;
    std::fs::write(&path, text).ok()?;
    Some(path)
}
