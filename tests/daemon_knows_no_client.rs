//! The daemon is mechanism its clients fill in (src/AGENTS.md): no client,
//! terminal or tool it serves is named in its source. A name found here is
//! knowledge that belongs to a client, such as Sonny, which passes it in.
use std::path::{Path, PathBuf};

/// The CLI's own files, which are a client: everything else under `src/` is
/// the daemon.
const CLIENT: [&str; 5] = [
    "src/cli.rs",
    "src/client.rs",
    "src/client_path.rs",
    "src/sign_in.rs",
    "src/main.rs",
];

/// Names only a client knows, matched as written.
const NAMES: [&str; 12] = [
    "sonny", "Sonny", "herdr", "Herdr", "HERDR", "cmux", "CMUX", "tmux", "TMUX", "REX_", "lookout",
    "Lookout",
];

fn sources(folder: &Path, found: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(folder).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(&path, found);
        } else if path.extension().is_some_and(|e| e == "rs") {
            found.push(path);
        }
    }
}

#[test]
fn the_daemon_names_no_client_terminal_or_tool() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    sources(&root.join("src"), &mut files);
    let mut named = Vec::new();
    for file in files {
        let relative = file
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        if CLIENT.contains(&relative.as_str()) {
            continue;
        }
        let text = std::fs::read_to_string(&file).unwrap();
        // A file's tests may name what they stand in for.
        let code = text.split("#[cfg(test)]").next().unwrap_or_default();
        for (line, content) in code.lines().enumerate() {
            for name in NAMES.iter().filter(|name| content.contains(*name)) {
                named.push(format!("{relative}:{}: {name}", line + 1));
            }
        }
    }
    assert!(
        named.is_empty(),
        "the daemon names what only a client knows; have the client pass it in:\n{}",
        named.join("\n")
    );
}
