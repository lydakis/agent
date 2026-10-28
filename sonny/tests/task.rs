//! The `start_task` and `remove_task` tools end to end, as the lookout
//! runs them (`sonny tool NAME`), with a stand-in for the daemon's
//! `agent`: a task gets a worktree by default and a sandfs copy when Sonny
//! is set to, is started there with what the lead asked for, and a start
//! that fails leaves no folder behind. The sandfs case mounts for real, so it needs what a
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
        self.run_with(dir, program, args, fail, None)
    }

    fn run_with(
        &self,
        dir: &Path,
        program: &str,
        args: &[&str],
        fail: bool,
        stdin: Option<&str>,
    ) -> (bool, String, String) {
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(dir)
            .env("PWD", dir)
            .env("AGENT_BOT", "demo.lead")
            .env("AGENT_BOT_ID", "1")
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
        if stdin.is_some() {
            command.stdin(std::process::Stdio::piped());
        }
        let mut child = command
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        if let Some(stdin) = stdin {
            use std::io::Write;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(stdin.as_bytes())
                .unwrap();
        }
        let output = child.wait_with_output().unwrap();
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

    /// A call of one of Sonny's tools, run in `dir` as the lookout runs it
    /// for the lead `demo.lead`: whether it was answered with a result, and
    /// the result, or the error's detail.
    fn tool(
        &self,
        dir: &Path,
        name: &str,
        arguments: serde_json::Value,
        fail: bool,
    ) -> (bool, String) {
        let (ok, out, err) = self.run_with(
            dir,
            env!("CARGO_BIN_EXE_sonny"),
            &["tool", name],
            fail,
            Some(&arguments.to_string()),
        );
        assert!(ok, "{err}");
        let answer: serde_json::Value =
            serde_json::from_str(&out).unwrap_or_else(|_| panic!("{out}\n{err}"));
        match answer["output"].as_str() {
            Some(output) => (true, output.to_owned()),
            None => {
                assert!(answer["error"]["code"].is_string(), "{answer}");
                (
                    false,
                    answer["error"]["detail"].as_str().unwrap().to_owned(),
                )
            }
        }
    }

    /// The lead starts demo.fix from its folder's `src`.
    fn task(&self, fail: bool, setup: bool) -> (bool, String) {
        let start = serde_json::json!({"name": "demo.fix", "brief": "fix it", "setup": setup});
        self.tool(&self.repo.join("src"), "start_task", start, fail)
    }

    fn remove(&self, dir: &Path, discard: bool) -> (bool, String) {
        let call = serde_json::json!({"task": "demo.fix", "discard": discard});
        self.tool(dir, "remove_task", call, false)
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
    let (ok, said) = lab.task(false, false);
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
    assert_eq!(ran[ran.len() - 2..], ["--", "fix it"]);
    let told = ran[expected.len()..ran.len() - 2].join("\n");
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
    let start = serde_json::json!({"name": "fix", "brief": brief});
    let (ok, said) = lab.tool(&lab.repo.join("src"), "start_task", start, false);
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
    let (ok, said) = lab.task(true, false);
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
    let (ok, said) = lab.task(false, false);
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
    let (ok, said) = lab.task(false, false);
    assert!(!ok, "{said}");
    assert!(said.contains("pick another name"), "{said}");
    assert!(!lab.scratch.join("run.log").exists());
    // One of this repository's, as a task of this name left it, is reused.
    std::fs::remove_dir_all(&worktree).unwrap();
    lab.git(
        &lab.repo,
        &["worktree", "add", "-q", "-b", "agent/demo.fix", &target],
    );
    let (ok, said) = lab.task(false, false);
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
    let (ok, said) = lab.task(false, true);
    assert!(ok, "{said}");
    let folder = lab.home.join(".agent/worktrees/demo.fix/src");
    let from = std::fs::read_to_string(folder.join("from")).unwrap();
    assert_eq!(from.trim(), lab.repo.join("src").to_string_lossy());
    // Setup is Sonny's, never the run's.
    assert!(!lab.ran().contains(&"--setup".to_owned()));
}

#[test]
fn a_sandfs_task_copy_takes_the_checkout_and_pushes_back() {
    let lab = Lab::new("sandfs");
    lab.sandfs();
    let (ok, said) = lab.task(false, false);
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
    let (ok, err) = lab.remove(&lab.repo, false);
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
    let (ok, err) = lab.remove(&lab.repo, false);
    assert!(!ok && err.contains("uncommitted changes"), "{err}");
    std::fs::remove_file(copy.join("new.rs")).unwrap();
    // So does a commit on no branch.
    lab.git(&copy, &["checkout", "-q", "--detach"]);
    lab.git(&copy, &["commit", "-q", "--allow-empty", "-m", "loose"]);
    let (ok, err) = lab.remove(&lab.repo, false);
    assert!(!ok && err.contains("on HEAD"), "{err}");
    // Left unmounted, as by a restart, it is mounted again to be checked.
    let sandfs = sandfs::Home::at(lab.home.join("sandfs")).unwrap();
    sandfs.unmount("demo.fix").unwrap();
    assert!(!copy.join(".git").exists());
    let (ok, err) = lab.remove(&lab.repo, false);
    assert!(!ok && err.contains("on HEAD"), "{err}");
    // A tag keeps it too, as does a stash.
    lab.git(&copy, &["tag", "-a", "-m", "kept", "kept"]);
    lab.git(&copy, &["switch", "-q", "agent/demo.fix"]);
    let (ok, err) = lab.remove(&lab.repo, false);
    assert!(!ok && err.contains("on kept"), "{err}");
    lab.git(&copy, &["tag", "-d", "kept"]);
    std::fs::write(copy.join("src/lib.rs"), "three\n").unwrap();
    lab.git(&copy, &["stash", "-q"]);
    let (ok, err) = lab.remove(&lab.repo, false);
    assert!(!ok && err.contains("on stash@{0}"), "{err}");
    lab.git(&copy, &["stash", "drop", "-q"]);
    // Unmounted, so the folder can go with the test.
    let (ok, err) = lab.remove(&lab.repo, false);
    assert!(ok, "{err}");
    assert!(sandfs.workspaces().unwrap().is_empty());
}

#[test]
fn a_sandfs_task_that_does_not_start_leaves_no_copy() {
    let lab = Lab::new("sandfs-fails");
    lab.sandfs();
    let (ok, said) = lab.task(true, false);
    assert!(!ok, "{said}");
    let sandfs = sandfs::Home::at(lab.home.join("sandfs")).unwrap();
    assert!(sandfs.workspaces().unwrap().is_empty());
}

#[test]
fn a_task_starts_with_the_agent_installed_beside_sonny() {
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
    let mut child = Command::new(&sonny)
        .args(["tool", "start_task"])
        .current_dir(&lab.repo)
        .env("HOME", &lab.home)
        .env("AGENT_BOT", "demo.lead")
        .env_remove("AGENT_BIN")
        .env("AGENT_LOG", lab.scratch.join("run.log"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    let start = serde_json::json!({"name": "fix", "brief": "fix it"});
    child
        .stdin
        .take()
        .unwrap()
        .write_all(start.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("\"output\""),
        "{output:?}"
    );
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
    let start = serde_json::json!({"name": "fix", "brief": "go"});
    let (ok, said) = lab.tool(&folder, "start_task", start, false);
    let copy = lab.home.join(".agent/tasks/demo.fix");
    let finish = |lab: &Lab| {
        let sandfs = sandfs::Home::at(lab.home.join("sandfs")).unwrap();
        let _ = sandfs.remove("demo.fix", true);
        let _ = std::fs::remove_dir_all(&folder);
    };
    if !ok {
        finish(&lab);
        panic!("{said}");
    }
    assert!(said.contains("another disk"), "{said}");
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
    let (ok, said) = lab.task(false, false);
    assert!(ok, "{said}");
    let worktree = lab.home.join(".agent/worktrees/demo.fix");
    std::fs::write(lab.scratch.join("run.log.ls"), r#"[{"name":"demo.fix"}]"#).unwrap();
    // Work not committed yet keeps it, and its agent too.
    std::fs::write(worktree.join("new.txt"), "x\n").unwrap();
    let (ok, err) = lab.remove(&lab.repo, false);
    assert!(!ok && err.contains("uncommitted changes"), "{err}");
    assert!(worktree.exists() && !lab.scratch.join("run.log.rm").exists());
    std::fs::remove_file(worktree.join("new.txt")).unwrap();
    let (ok, err) = lab.remove(&lab.repo, false);
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
        let start = serde_json::json!({"name": "fix", "brief": "go"});
        let (ok, said) = lab.tool(&folder, "start_task", start, false);
        assert!(ok, "{said}");
        assert!(
            said.contains("Git is off in its copy of 1 linked worktrees"),
            "{said}"
        );
        assert!(said.contains("without asking"), "{said}");
        // Git stops in the copy's worktree, and works in the real one.
        // Which words git says it with depend on its version; the reason is
        // in the .git file it stops at.
        let wt = copy.join("shop/.worktrees/wt");
        let (ok, _, said) = lab.run(&wt, "git", &["status"], false);
        assert!(!ok, "{said}");
        let reason = std::fs::read_to_string(wt.join(".git")).unwrap();
        assert!(reason.contains("git is off here"), "{reason}");
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
        let (ok, err) = lab.remove(&folder, false);
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

        let (ok, err) = lab.remove(&folder, false);
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
