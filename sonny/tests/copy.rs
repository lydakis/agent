//! `sonny copy` end to end: a task's copy of a git checkout, mounted by
//! Sonny itself, with the checkout's uncommitted change and a branch of its
//! own; its commits pushed back; then removed. Needs what a mount needs:
//! FUSE 3 on Linux, nothing on macOS.
use std::path::Path;
use std::process::Command;

fn run(dir: &Path, home: &Path, program: &str, args: &[&str]) -> (bool, String, String) {
    let output = Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("SANDFS_HOME", home.join("sandfs"))
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        // Graft, when installed, has no project here.
        .env("GRAFT_HOME", home.join("graft"))
        .output()
        .unwrap();
    let text = |bytes: Vec<u8>| String::from_utf8_lossy(&bytes).trim().to_owned();
    (
        output.status.success(),
        text(output.stdout),
        text(output.stderr),
    )
}

#[test]
fn a_task_copy_of_a_checkout_takes_its_changes_and_pushes_back() {
    let scratch = std::env::temp_dir().join(format!("sonny-copy-{}", std::process::id()));
    let (home, repo) = (scratch.join("home"), scratch.join("repo"));
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let home = std::fs::canonicalize(home).unwrap();
    let repo = std::fs::canonicalize(repo).unwrap();
    let git = |args: &[&str]| {
        let (ok, out, err) = run(&repo, &home, "git", args);
        assert!(ok, "git {args:?}: {err}");
        out
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("src/lib.rs"), "one\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "one"]);
    std::fs::write(repo.join("src/lib.rs"), "two\n").unwrap();

    let sonny = env!("CARGO_BIN_EXE_sonny");
    // From a subfolder, the task starts in the same subfolder of its copy.
    let (ok, dir, note) = run(&repo.join("src"), &home, sonny, &["copy", "demo.fix"]);
    assert!(ok, "sonny copy: {note}");
    let copy = home.join(".agent/tasks/demo.fix");
    assert_eq!(Path::new(&dir), copy.join("src"));
    assert!(note.contains("agent/demo.fix"), "{note}");
    assert_eq!(
        std::fs::read_to_string(copy.join("src/lib.rs")).unwrap(),
        "two\n"
    );
    // Asking again finds the copy as it is.
    let (ok, again, _) = run(&repo, &home, sonny, &["copy", "demo.fix"]);
    assert!(ok);
    // Exactly the folder, with no trailing slash.
    assert_eq!(again, copy.to_string_lossy());

    let (_, branch, _) = run(&copy, &home, "git", &["branch", "--show-current"]);
    assert_eq!(branch, "agent/demo.fix");
    let (_, listed, _) = run(&repo, &home, sonny, &["copy", "changes", "demo.fix"]);
    assert_eq!(listed, "M src/lib.rs");
    for args in [
        &["commit", "-qam", "two"][..],
        &["push", "-q", "origin", "agent/demo.fix"],
    ] {
        let (ok, _, err) = run(&copy, &home, "git", args);
        assert!(ok, "git {args:?} in the copy: {err}");
    }
    assert_eq!(git(&["show", "agent/demo.fix:src/lib.rs"]), "two");

    let (ok, _, err) = run(&repo, &home, sonny, &["copy", "rm", "demo.fix"]);
    assert!(ok, "sonny copy rm: {err}");
    assert!(!copy.join("src").exists());
    let _ = std::fs::remove_dir_all(&scratch);
}
