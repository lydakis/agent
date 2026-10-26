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
        let scratch =
            std::env::temp_dir().join(format!("sonny-task-{test}-{}", std::process::id()));
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
             ls) echo '[]';;\nesac\n",
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
        "--agents",
        "--",
        "fix",
        "it",
    ];
    assert_eq!(lab.ran(), expected);
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
    lab.git(&copy, &["push", "-q", "origin", "agent/demo.fix"]);
    assert_eq!(
        lab.git(&lab.repo, &["show", "agent/demo.fix:src/lib.rs"]),
        "two"
    );
    // Unmounted, so the folder can go with the test.
    let sandfs = sandfs::Home::at(lab.home.join("sandfs")).unwrap();
    sandfs.remove("demo.fix", true).unwrap();
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
