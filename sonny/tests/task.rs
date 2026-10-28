//! `sonny task` end to end, with a stand-in for the daemon's `agent`: a
//! task gets a worktree by default and a sandfs copy when Sonny is set to,
//! is started there with the lead's flags, and a start that fails leaves
//! no folder behind. The sandfs case mounts for real, so it needs what a
//! mount needs: FUSE 3 on Linux, nothing on macOS.
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Lab {
    scratch: PathBuf,
    home: PathBuf,
    repo: PathBuf,
    agent: PathBuf,
}

impl Lab {
    /// A repository with a commit and an uncommitted edit under `src`,
    /// and an `agent` that logs what `run` was given.
    fn new(test: &str) -> Lab {
        // Under /tmp, not macOS's long $TMPDIR: sandfs's mount socket lives
        // in the home made here, and a socket path must fit in 104 bytes.
        let scratch = Path::new("/tmp").join(format!("sonny-task-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        for dir in ["home", "repo/src"] {
            std::fs::create_dir_all(scratch.join(dir)).unwrap();
        }
        let scratch = std::fs::canonicalize(scratch).unwrap();
        let agent = scratch.join("agent");
        std::fs::write(
            &agent,
            "#!/bin/sh\ncase \"$1\" in\n\
             run) printf '%s\\n' \"$@\" > \"$AGENT_LOG\"; [ -z \"$AGENT_FAIL\" ] || exit 1; echo '{\"handle\":\"h\"}';;\n\
             ls) cat \"$AGENT_LOG.ls\" 2>/dev/null || echo '[]';;\n\
             rm) printf '%s\\n' \"$@\" > \"$AGENT_LOG.rm\";;\nesac\n",
        )
        .unwrap();
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
        let lab = Lab {
            home: scratch.join("home"),
            repo: scratch.join("repo"),
            scratch,
            agent,
        };
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.name", "t"],
            &["config", "user.email", "t@example.com"],
        ] {
            lab.git(&lab.repo, args);
        }
        std::fs::write(lab.repo.join("src/lib.rs"), "one\n").unwrap();
        lab.git(&lab.repo, &["add", "."]);
        lab.git(&lab.repo, &["commit", "-qm", "one"]);
        std::fs::write(lab.repo.join("src/lib.rs"), "two\n").unwrap();
        lab
    }

    fn sandfs(&self) {
        let settings = self.home.join(".agent/sonny");
        std::fs::create_dir_all(&settings).unwrap();
        std::fs::write(settings.join("settings.json"), r#"{"copies": "sandfs"}"#).unwrap();
    }

    fn run(&self, dir: &Path, program: &str, args: &[&str], fail: bool) -> (bool, String, String) {
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(dir)
            .env("HOME", &self.home)
            .env("SANDFS_HOME", self.home.join("sandfs"))
            .env("AGENT_BIN", &self.agent)
            .env("AGENT_LOG", self.scratch.join("run.log"))
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com");
        if fail {
            command.env("AGENT_FAIL", "1");
        }
        let output = command.output().unwrap();
        let text = |bytes: Vec<u8>| String::from_utf8_lossy(&bytes).trim().to_owned();
        (
            output.status.success(),
            text(output.stdout),
            text(output.stderr),
        )
    }

    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let (ok, out, err) = self.run(dir, "git", args, false);
        assert!(ok, "git {args:?}: {err}");
        out
    }

    fn task(&self, fail: bool, extra: &[&str]) -> (bool, String) {
        let mut args = vec!["task", "demo.fix", "--agents"];
        args.extend(extra);
        args.extend(["--", "fix", "it"]);
        let (ok, out, err) = self.run(
            &self.repo.join("src"),
            env!("CARGO_BIN_EXE_sonny"),
            &args,
            fail,
        );
        (ok, format!("{out}\n{err}"))
    }

    /// What `agent run` was given, one argument a line.
    fn ran(&self) -> Vec<String> {
        let log = std::fs::read_to_string(self.scratch.join("run.log")).unwrap();
        log.lines().map(str::to_owned).collect()
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.scratch);
    }
}

#[test]
fn a_task_starts_in_a_worktree_of_its_own_by_default() {
    let lab = Lab::new("worktree");
    let (ok, said) = lab.task(false, &[]);
    assert!(ok, "{said}");
    let worktree = lab.home.join(".agent/worktrees/demo.fix");
    let workspace = worktree.join("src").to_string_lossy().into_owned();
    let expected = [
        "run",
        "--detach",
        "--new",
        "--bot",
        "demo.fix",
        "--workspace",
        &workspace,
        "--instructions",
    ];
    // What --agents composes, given as the task's instructions, with who
    // it works for after it.
    let ran = lab.ran();
    assert_eq!(ran[..expected.len()], expected);
    assert_eq!(ran[ran.len() - 3..], ["--", "fix", "it"]);
    let told = ran[expected.len()..ran.len() - 3].join("\n");
    assert!(told.contains("The person you work for"), "{told}");
    assert!(said.contains("\"handle\""), "{said}");
    // From the last commit, on its own branch.
    assert_eq!(
        std::fs::read_to_string(worktree.join("src/lib.rs")).unwrap(),
        "one\n"
    );
    assert_eq!(
        lab.git(&worktree, &["branch", "--show-current"]),
        "agent/demo.fix"
    );
}

#[test]
fn a_brief_that_names_the_lead_s_folder_names_the_task_s_copy() {
    let lab = Lab::new("brief");
    // A link in the home to the repository, as `~/workplace` is to
    // `/Volumes/workplace`.
    std::os::unix::fs::symlink(&lab.repo, lab.home.join("link")).unwrap();
    let repo = lab.repo.to_string_lossy();
    let brief = format!("edit {repo}/src/lib.rs and ~/link/src, keep {repo}2/x");
    let (ok, _, said) = lab.run(
        &lab.repo.join("src"),
        env!("CARGO_BIN_EXE_sonny"),
        &["task", "demo.fix", "--", &brief],
        false,
    );
    assert!(ok, "{said}");
    let copy = lab.home.join(".agent/worktrees/demo.fix");
    let copy = copy.to_string_lossy();
    assert_eq!(
        lab.ran().last().unwrap(),
        &format!("edit {copy}/src/lib.rs and {copy}/src, keep {repo}2/x")
    );
    assert!(said.contains("2 times"), "{said}");
}

#[test]
fn a_task_that_does_not_start_leaves_no_worktree_or_branch() {
    let lab = Lab::new("worktree-fails");
    let (ok, said) = lab.task(true, &[]);
    assert!(!ok, "{said}");
    assert!(!lab.home.join(".agent/worktrees/demo.fix").exists());
    assert_eq!(
        lab.git(&lab.repo, &["branch", "--list", "agent/demo.fix"]),
        ""
    );
}

#[test]
fn a_task_whose_agent_cannot_run_leaves_no_worktree() {
    let mut lab = Lab::new("no-agent");
    lab.agent = lab.scratch.join("missing");
    let (ok, said) = lab.task(false, &[]);
    assert!(!ok, "{said}");
    assert!(!lab.home.join(".agent/worktrees/demo.fix").exists());
}

#[test]
fn a_worktree_left_by_another_repository_is_not_taken_over() {
    let lab = Lab::new("other-repo");
    let other = lab.scratch.join("other");
    std::fs::create_dir_all(&other).unwrap();
    lab.git(&other, &["init", "-q", "-b", "main"]);
    lab.git(&other, &["commit", "-q", "--allow-empty", "-m", "other"]);
    let worktree = lab.home.join(".agent/worktrees/demo.fix");
    let target = worktree.to_string_lossy().into_owned();
    lab.git(
        &other,
        &["worktree", "add", "-q", "-b", "agent/demo.fix", &target],
    );
    let (ok, said) = lab.task(false, &[]);
    assert!(!ok, "{said}");
    assert!(said.contains("pick another NAME"), "{said}");
    assert!(!lab.scratch.join("run.log").exists());
    // One of this repository's, as a task of this name left it, is reused.
    std::fs::remove_dir_all(&worktree).unwrap();
    lab.git(
        &lab.repo,
        &["worktree", "add", "-q", "-b", "agent/demo.fix", &target],
    );
    let (ok, said) = lab.task(false, &[]);
    assert!(ok, "{said}");
}

#[test]
fn setup_runs_in_the_tasks_folder_and_a_failing_one_starts_nothing() {
    let lab = Lab::new("setup");
    let setup = lab.repo.join("src/.agents/setup");
    std::fs::create_dir_all(setup.parent().unwrap()).unwrap();
    std::fs::write(
        &setup,
        "#!/bin/sh\necho \"$AGENT_SOURCE\" > from\nexit \"${SETUP_EXIT:-0}\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&setup, std::fs::Permissions::from_mode(0o755)).unwrap();
    lab.git(&lab.repo, &["add", "."]);
    lab.git(&lab.repo, &["commit", "-qm", "setup"]);
    let (ok, said) = lab.task(false, &["--setup"]);
    assert!(ok, "{said}");
    let folder = lab.home.join(".agent/worktrees/demo.fix/src");
    let from = std::fs::read_to_string(folder.join("from")).unwrap();
    assert_eq!(from.trim(), lab.repo.join("src").to_string_lossy());
    // --setup is Sonny's, never the run's.
    assert!(!lab.ran().contains(&"--setup".to_owned()));
}

#[test]
fn a_sandfs_task_copy_takes_the_checkout_and_pushes_back() {
    let lab = Lab::new("sandfs");
    lab.sandfs();
    let (ok, said) = lab.task(false, &[]);
    assert!(ok, "{said}");
    let copy = lab.home.join(".agent/tasks/demo.fix");
    assert_eq!(lab.ran()[6], copy.join("src").to_string_lossy());
    assert!(said.contains("agent/demo.fix"), "{said}");
    // The uncommitted edit came along, on a branch of the task's own.
    assert_eq!(
        std::fs::read_to_string(copy.join("src/lib.rs")).unwrap(),
        "two\n"
    );
    assert_eq!(
        lab.git(&copy, &["branch", "--show-current"]),
        "agent/demo.fix"
    );
    let sonny = env!("CARGO_BIN_EXE_sonny");
    let (_, listed, _) = lab.run(&lab.repo, sonny, &["task", "changes", "demo.fix"], false);
    assert_eq!(listed, "M src/lib.rs");
    lab.git(&copy, &["commit", "-qam", "two"]);
    // A commit only in the copy keeps the task, on whichever branch.
    lab.git(&copy, &["checkout", "-q", "--detach", "HEAD~1"]);
    let (ok, _, err) = lab.run(&lab.repo, sonny, &["task", "rm", "demo.fix"], false);
    assert!(!ok && err.contains("commits not pushed"), "{err}");
    assert!(err.contains("on agent/demo.fix"), "{err}");
    lab.git(&copy, &["switch", "-q", "agent/demo.fix"]);
    lab.git(&copy, &["push", "-q", "origin", "agent/demo.fix"]);
    assert_eq!(
        lab.git(&lab.repo, &["show", "agent/demo.fix:src/lib.rs"]),
        "two"
    );
    // A new file, even one git status is set not to show, keeps it.
    lab.git(&copy, &["config", "status.showUntrackedFiles", "no"]);
    std::fs::write(copy.join("new.rs"), "new\n").unwrap();
    let (ok, _, err) = lab.run(&lab.repo, sonny, &["task", "rm", "demo.fix"], false);
    assert!(!ok && err.contains("uncommitted changes"), "{err}");
    std::fs::remove_file(copy.join("new.rs")).unwrap();
    // So does a commit on no branch.
    lab.git(&copy, &["checkout", "-q", "--detach"]);
    lab.git(&copy, &["commit", "-q", "--allow-empty", "-m", "loose"]);
    let (ok, _, err) = lab.run(&lab.repo, sonny, &["task", "rm", "demo.fix"], false);
    assert!(!ok && err.contains("on HEAD"), "{err}");
    // Left unmounted, as by a restart, it is mounted again to be checked.
    let sandfs = sandfs::Home::at(lab.home.join("sandfs")).unwrap();
    sandfs.unmount("demo.fix").unwrap();
    assert!(!copy.join(".git").exists());
    let (ok, _, err) = lab.run(&lab.repo, sonny, &["task", "rm", "demo.fix"], false);
    assert!(!ok && err.contains("on HEAD"), "{err}");
    // A tag keeps it too, as does a stash.
    lab.git(&copy, &["tag", "-a", "-m", "kept", "kept"]);
    lab.git(&copy, &["switch", "-q", "agent/demo.fix"]);
    let (ok, _, err) = lab.run(&lab.repo, sonny, &["task", "rm", "demo.fix"], false);
    assert!(!ok && err.contains("on kept"), "{err}");
    lab.git(&copy, &["tag", "-d", "kept"]);
    std::fs::write(copy.join("src/lib.rs"), "three\n").unwrap();
    lab.git(&copy, &["stash", "-q"]);
    let (ok, _, err) = lab.run(&lab.repo, sonny, &["task", "rm", "demo.fix"], false);
    assert!(!ok && err.contains("on stash@{0}"), "{err}");
    lab.git(&copy, &["stash", "drop", "-q"]);
    // Unmounted, so the folder can go with the test.
    let (ok, _, err) = lab.run(&lab.repo, sonny, &["task", "rm", "demo.fix"], false);
    assert!(ok, "{err}");
    assert!(sandfs.workspaces().unwrap().is_empty());
}

#[test]
fn a_sandfs_task_that_does_not_start_leaves_no_copy() {
    let lab = Lab::new("sandfs-fails");
    lab.sandfs();
    let (ok, said) = lab.task(true, &[]);
    assert!(!ok, "{said}");
    let sandfs = sandfs::Home::at(lab.home.join("sandfs")).unwrap();
    assert!(sandfs.workspaces().unwrap().is_empty());
}

#[test]
fn a_task_starts_from_a_plain_terminal_with_the_agent_beside_sonny() {
    let lab = Lab::new("terminal");
    // Installed as Homebrew lays it out: bin/sonny, and libexec/agent.
    for dir in ["bin", "libexec"] {
        std::fs::create_dir_all(lab.scratch.join(dir)).unwrap();
    }
    let sonny = lab.scratch.join("bin/sonny");
    if std::fs::hard_link(env!("CARGO_BIN_EXE_sonny"), &sonny).is_err() {
        std::fs::copy(env!("CARGO_BIN_EXE_sonny"), &sonny).unwrap();
    }
    std::fs::rename(&lab.agent, lab.scratch.join("libexec/agent")).unwrap();
    let output = Command::new(&sonny)
        .args(["task", "demo.fix", "--", "fix", "it"])
        .current_dir(&lab.repo)
        .env("HOME", &lab.home)
        .env_remove("AGENT_BIN")
        .env("AGENT_LOG", lab.scratch.join("run.log"))
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(lab.ran()[4], "demo.fix");
}

#[test]
fn a_plain_folder_on_another_disk_gets_a_sandfs_workspace_over_it() {
    // /dev/shm is a disk of its own on Linux; elsewhere there is no
    // second disk to try this on.
    let shm = Path::new("/dev/shm");
    if !shm.is_dir() {
        return;
    }
    let lab = Lab::new("sandfs-disk");
    lab.sandfs();
    let folder = shm.join(format!("sonny-task-plain-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&folder);
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("notes.txt"), "one\n").unwrap();
    let sonny = env!("CARGO_BIN_EXE_sonny");
    let (ok, out, err) = lab.run(&folder, sonny, &["task", "demo.fix", "--", "go"], false);
    let copy = lab.home.join(".agent/tasks/demo.fix");
    let finish = |lab: &Lab| {
        let sandfs = sandfs::Home::at(lab.home.join("sandfs")).unwrap();
        let _ = sandfs.remove("demo.fix", true);
        let _ = std::fs::remove_dir_all(&folder);
    };
    if !ok {
        finish(&lab);
        panic!("{out}\n{err}");
    }
    assert!(err.contains("another disk"), "{err}");
    assert_eq!(lab.ran()[6], copy.to_string_lossy());
    // The task's edit stays in its workspace until applied.
    std::fs::write(copy.join("notes.txt"), "two\n").unwrap();
    let (_, listed, _) = lab.run(&folder, sonny, &["task", "changes", "demo.fix"], false);
    let source = std::fs::read_to_string(folder.join("notes.txt")).unwrap();
    finish(&lab);
    assert_eq!(listed, "M notes.txt");
    assert_eq!(source, "one\n");
}

#[test]
fn a_done_worktree_task_is_removed_with_its_merged_branch() {
    let lab = Lab::new("rm-worktree");
    let (ok, said) = lab.task(false, &[]);
    assert!(ok, "{said}");
    let worktree = lab.home.join(".agent/worktrees/demo.fix");
    std::fs::write(lab.scratch.join("run.log.ls"), r#"[{"name":"demo.fix"}]"#).unwrap();
    let sonny = env!("CARGO_BIN_EXE_sonny");
    // Work not committed yet keeps it, and its agent too.
    std::fs::write(worktree.join("new.txt"), "x\n").unwrap();
    let (ok, _, err) = lab.run(&lab.repo, sonny, &["task", "rm", "demo.fix"], false);
    assert!(!ok && err.contains("uncommitted changes"), "{err}");
    assert!(worktree.exists() && !lab.scratch.join("run.log.rm").exists());
    std::fs::remove_file(worktree.join("new.txt")).unwrap();
    let (ok, _, err) = lab.run(&lab.repo, sonny, &["task", "rm", "demo.fix"], false);
    assert!(ok, "{err}");
    assert!(!worktree.exists());
    let removed = std::fs::read_to_string(lab.scratch.join("run.log.rm")).unwrap();
    assert_eq!(removed, "rm\n--bot\ndemo.fix\n");
    // Its branch had nothing more than this one, so it went too.
    assert_eq!(
        lab.git(&lab.repo, &["branch", "--list", "agent/demo.fix"]),
        ""
    );
}

#[test]
fn a_plain_folder_copy_turns_git_off_in_linked_worktrees_and_applies_what_it_lists() {
    let shm = Path::new("/dev/shm");
    if !shm.is_dir() {
        return;
    }
    let lab = Lab::new("sandfs-linked");
    lab.sandfs();
    let folder = shm.join(format!("sonny-task-linked-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&folder);
    let shop = folder.join("shop");
    std::fs::create_dir_all(&shop).unwrap();
    for (path, text) in [
        ("notes.txt", "one\n"),
        ("old.txt", "old\n"),
        ("shop/a.py", "a\n"),
        ("shop/.gitignore", "__pycache__/\n"),
    ] {
        std::fs::write(folder.join(path), text).unwrap();
    }
    lab.git(&shop, &["init", "-q", "-b", "main"]);
    lab.git(&shop, &["add", "."]);
    lab.git(&shop, &["commit", "-qm", "one"]);
    lab.git(
        &shop,
        &["worktree", "add", "-q", ".worktrees/wt", "-b", "wt"],
    );
    let sonny = env!("CARGO_BIN_EXE_sonny");
    let copy = lab.home.join(".agent/tasks/demo.fix");
    let finish = |lab: &Lab| {
        let sandfs = sandfs::Home::at(lab.home.join("sandfs")).unwrap();
        let _ = sandfs.remove("demo.fix", true);
        let _ = std::fs::remove_dir_all(&folder);
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let (ok, out, err) = lab.run(&folder, sonny, &["task", "demo.fix", "--", "go"], false);
        assert!(ok, "{out}\n{err}");
        assert!(
            err.contains("Git is off in its copy of 1 linked worktrees"),
            "{err}"
        );
        assert!(err.contains("without asking"), "{err}");
        // Git stops in the copy's worktree, and works in the real one.
        let (ok, _, said) = lab.run(&copy.join("shop/.worktrees/wt"), "git", &["status"], false);
        assert!(
            !ok && (said.contains("git is off here")
                || said.contains("gitfile does not point to a valid repository")),
            "{said}"
        );
        lab.git(&shop.join(".worktrees/wt"), &["status"]);

        std::fs::write(copy.join("notes.txt"), "two\n").unwrap();
        std::fs::remove_file(copy.join("old.txt")).unwrap();
        std::fs::create_dir_all(copy.join("shop/__pycache__")).unwrap();
        std::fs::write(copy.join("shop/__pycache__/a.pyc"), "c").unwrap();
        let (_, listed, err) = lab.run(&folder, sonny, &["task", "changes", "demo.fix"], false);
        let mut lines: Vec<&str> = listed.lines().collect();
        lines.sort();
        assert_eq!(lines, ["D old.txt", "M notes.txt"], "{err}");
        assert!(err.contains("1 files git ignores not listed"), "{err}");
        assert!(err.contains("git's own files not listed"), "{err}");

        // Not here yet, so not removed.
        let (ok, _, err) = lab.run(&folder, sonny, &["task", "rm", "demo.fix"], false);
        assert!(!ok && err.contains("not applied here"), "{err}");
        let args = [
            "task",
            "changes",
            "demo.fix",
            "--apply",
            "--only",
            "notes.txt",
        ];
        let (ok, _, err) = lab.run(&folder, sonny, &args, false);
        assert!(ok, "{err}");
        assert_eq!(
            std::fs::read_to_string(folder.join("notes.txt")).unwrap(),
            "two\n"
        );
        assert!(folder.join("old.txt").exists());
        let args = ["task", "changes", "demo.fix", "--apply"];
        let (ok, _, err) = lab.run(&folder, sonny, &args, false);
        assert!(ok, "{err}");
        assert!(!folder.join("old.txt").exists());
        assert!(!folder.join("shop/__pycache__").exists());
        // The real worktree keeps its own .git.
        let real = std::fs::read_to_string(shop.join(".worktrees/wt/.git")).unwrap();
        assert!(!real.contains("git is off"), "{real}");

        let (ok, _, err) = lab.run(&folder, sonny, &["task", "rm", "demo.fix"], false);
        assert!(ok, "{err}");
        assert!(!copy.exists());
        let sandfs = sandfs::Home::at(lab.home.join("sandfs")).unwrap();
        assert!(sandfs.workspaces().unwrap().is_empty());
    }));
    finish(&lab);
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
