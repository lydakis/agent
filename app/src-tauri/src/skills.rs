//! Skills the app ships, written into `~/.agents/skills` whenever it starts,
//! where every agent composed with `--agents` finds them. A folder becomes
//! yours once you change it: the app replaces only one it wrote and nobody
//! has edited since, which its `.shipped` file records as a hash of what the
//! app wrote. A folder of the same name the app did not write is left alone.
use std::path::Path;

type Files = &'static [(&'static str, &'static str)];

const SHIPPED: [(&str, Files); 1] = [(
    "workflow",
    &[
        ("SKILL.md", include_str!("../../skills/workflow/SKILL.md")),
        (
            "workflow.py",
            include_str!("../../skills/workflow/workflow.py"),
        ),
    ],
)];
const RECORD: &str = ".shipped";

pub fn install(home: &Path) -> Result<(), String> {
    for (name, files) in SHIPPED {
        install_one(&home.join(".agents/skills").join(name), files)?;
    }
    Ok(())
}

fn install_one(dir: &Path, files: Files) -> Result<(), String> {
    let ours = format!(
        "{:016x}\n",
        hash(files.iter().map(|(n, t)| (*n, t.as_bytes().to_vec())))
    );
    if dir.exists() {
        let Ok(recorded) = std::fs::read_to_string(dir.join(RECORD)) else {
            return Ok(());
        };
        if recorded == ours {
            return Ok(());
        }
        let now = files.iter().map(|(n, _)| {
            let read = std::fs::read(dir.join(n)).unwrap_or_default();
            (*n, read)
        });
        if format!("{:016x}\n", hash(now)) != recorded {
            return Ok(());
        }
    }
    for (file, text) in files {
        crate::schedule::replace_mode(&dir.join(file), text, 0o644)?;
    }
    // Last, so a write cut short is redone at the next start.
    crate::schedule::replace_mode(&dir.join(RECORD), &ours, 0o644)
}

/// FNV-1a over each file's name and bytes: the same on every build, unlike
/// the standard library's hasher.
fn hash<'a>(files: impl Iterator<Item = (&'a str, Vec<u8>)>) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for (name, bytes) in files {
        let length = (bytes.len() as u64).to_le_bytes();
        for byte in name.bytes().chain(length).chain(bytes) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD: Files = &[("SKILL.md", "old skill"), ("workflow.py", "old runner")];
    const NEW: Files = &[("SKILL.md", "new skill"), ("workflow.py", "new runner")];

    #[test]
    fn a_shipped_skill_is_written_updated_and_left_alone_once_yours() {
        let root = std::env::temp_dir().join(format!("agent-app-skills-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("workflow");
        let read = |f: &str| std::fs::read_to_string(dir.join(f)).unwrap();

        install_one(&dir, OLD).unwrap();
        assert_eq!(read("SKILL.md"), "old skill");
        // Untouched since the app wrote it: the next version replaces it.
        install_one(&dir, NEW).unwrap();
        assert_eq!(
            (read("SKILL.md"), read("workflow.py")),
            ("new skill".into(), "new runner".into())
        );
        // Edited: it is yours, and stays as you left it.
        std::fs::write(dir.join("workflow.py"), "my runner").unwrap();
        install_one(&dir, OLD).unwrap();
        assert_eq!(
            (read("SKILL.md"), read("workflow.py")),
            ("new skill".into(), "my runner".into())
        );
        // A folder of that name the app never wrote is never touched.
        let foreign = root.join("foreign");
        std::fs::create_dir_all(&foreign).unwrap();
        std::fs::write(foreign.join("SKILL.md"), "mine").unwrap();
        install_one(&foreign, NEW).unwrap();
        assert_eq!(
            std::fs::read_to_string(foreign.join("SKILL.md")).unwrap(),
            "mine"
        );
        assert!(!foreign.join("workflow.py").exists());
        // Deleted: written again.
        std::fs::remove_dir_all(&dir).unwrap();
        install_one(&dir, NEW).unwrap();
        assert_eq!(read("SKILL.md"), "new skill");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_hash_is_stable_and_separates_names_from_contents() {
        let one = hash([("a", b"bc".to_vec())].into_iter());
        assert_eq!(one, hash([("a", b"bc".to_vec())].into_iter()));
        assert_ne!(one, hash([("ab", b"c".to_vec())].into_iter()));
    }
}
