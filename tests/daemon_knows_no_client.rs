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

/// A file's code before its tests: up to the first `#[cfg(test)]` that
/// opens a module with a body. One on a single item (a test-only helper,
/// `mod doubling;`) has daemon code after it, which is read too.
fn code(text: &str) -> &str {
    const TESTS: &str = "#[cfg(test)]";
    let mut from = 0;
    while let Some(at) = text[from..].find(TESTS).map(|at| from + at) {
        let next = text[at + TESTS.len()..].trim_start();
        let line = next.lines().next().unwrap_or_default().trim_end();
        let module = line.starts_with("mod ") || line.starts_with("pub(crate) mod ");
        if module && line.ends_with('{') {
            return &text[..at];
        }
        from = at + TESTS.len();
    }
    text
}

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
        let code = code(&text);
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

#[test]
fn only_a_test_module_ends_a_files_code() {
    let text = "#[cfg(test)]\nmod doubling;\nfn a() {}\n#[cfg(test)]\nfn only_tests() {}\nfn b() {}\n#[cfg(test)]\nmod tests {\n    fn c() {}\n}\n";
    let read = code(text);
    assert!(read.contains("fn a()") && read.contains("fn b()"), "{read}");
    assert!(!read.contains("fn c()"), "{read}");
}
