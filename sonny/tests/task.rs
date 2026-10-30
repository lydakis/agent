//! The `start_task` and `remove_task` tools end to end, as the lookout
//! runs them (`sonny tool NAME`), with a stand-in for the daemon's
//! `agent`: a task gets a worktree, is started there with what the lead
//! asked for, and a start that fails leaves no folder behind.
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
             rm) printf '%s\\n' \"$@\" > \"$AGENT_LOG.rm\";;\n\
             fork) printf '%s\\n' \"$@\" > \"$AGENT_LOG.fork\";;\nesac\n",
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
            .env("AGENT_BIN", &self.agent)
            .env("AGENT_LOG", self.scratch.join("run.log"))
            .env("AGENT_STORE", self.store())
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            // Keys the lookout that runs these calls may hold.
            .env("OPENAI_API_KEY", "sk-lab")
            .env("LAB_TOKEN", "t-lab")
            .env("AWS_SECRET_ACCESS_KEY", "aws-lab");
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

    /// The daemon's store the lookout and these calls name.
    fn store(&self) -> PathBuf {
        self.scratch.join("state.sqlite")
    }

    /// Where the store's snapshots are kept: under the FNV-1a hash of its
    /// path, as snapshot.rs names it.
    fn refs(&self) -> String {
        let hash = (self.store().as_os_str().as_encoded_bytes().iter())
            .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
                (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
            });
        format!("refs/agent/snapshots/{hash:016x}")
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
        "#!/bin/sh\necho \"$AGENT_SOURCE\" > from\n\
         echo \"${OPENAI_API_KEY:-none} ${LAB_TOKEN:-none} ${AWS_SECRET_ACCESS_KEY:-none}\" > keys\nexit \"${SETUP_EXIT:-0}\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(lab.home.join(".agent")).unwrap();
    let env = lab.home.join(".agent/env");
    std::fs::write(&env, "LAB_TOKEN=t-lab\n").unwrap();
    std::fs::set_permissions(&env, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::set_permissions(&setup, std::fs::Permissions::from_mode(0o755)).unwrap();
    lab.git(&lab.repo, &["add", "."]);
    lab.git(&lab.repo, &["commit", "-qm", "setup"]);
    let (ok, said) = lab.task(false, true);
    assert!(ok, "{said}");
    let folder = lab.home.join(".agent/worktrees/demo.fix/src");
    let from = std::fs::read_to_string(folder.join("from")).unwrap();
    assert_eq!(from.trim(), lab.repo.join("src").to_string_lossy());
    // The folder's own code sees none of the keys Agent keeps.
    let keys = std::fs::read_to_string(folder.join("keys")).unwrap();
    assert_eq!(keys.trim(), "none none none");
    // Setup is Sonny's, never the run's.
    assert!(!lab.ran().contains(&"--setup".to_owned()));
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
fn a_task_outside_git_is_removed_with_no_snapshots_to_drop() {
    let lab = Lab::new("rm-outside");
    let workspace = lab.scratch.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let start = serde_json::json!({"name": "demo.fix", "brief": "fix it"});
    let (ok, said) = lab.tool(&workspace, "start_task", start, false);
    assert!(ok, "{said}");
    let listed = serde_json::json!([{"name": "demo.fix", "bot_id": 5, "workspace": workspace}]);
    std::fs::write(lab.scratch.join("run.log.ls"), listed.to_string()).unwrap();
    let (ok, said) = lab.remove(&workspace, false);
    assert!(ok, "{said}");
    assert!(lab.scratch.join("run.log.rm").exists());
}

/// Keep the worktree's files as the lookout does after a completed turn
/// (snapshot.rs): bot 5's turn `turn`, at history node `checkpoint`.
fn snapshot(lab: &Lab, worktree: &Path, turn: i64, checkpoint: i64) {
    snapshot_in(lab, worktree, turn, checkpoint, None);
}

/// As `snapshot`, naming `folder` as the one its turn worked in.
fn snapshot_in(lab: &Lab, worktree: &Path, turn: i64, checkpoint: i64, folder: Option<&str>) {
    let index = lab.scratch.join("snapshot.index");
    let add = |args: &[&str]| {
        let output = Command::new("git")
            .arg("-C")
            .arg(worktree)
            .args(args)
            .env("GIT_INDEX_FILE", &index)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    };
    add(&["read-tree", "HEAD"]);
    add(&["add", "-A"]);
    let tree = add(&["write-tree"]);
    let mut message = format!("Files after turn {turn}\n\nAgent-Checkpoint: {checkpoint}");
    if let Some(folder) = folder {
        message.push_str(&format!("\nAgent-Folder: {folder}"));
    }
    let commit = add(&["commit-tree", &tree, "-p", "HEAD", "-m", &message]);
    lab.git(
        worktree,
        &["update-ref", &format!("{}/5/{turn}", lab.refs()), &commit],
    );
}

#[test]
fn a_fork_is_made_in_its_source_s_repository_from_a_lead_folder_outside_git() {
    let lab = Lab::new("fork-nested");
    let (ok, said) = lab.task(false, false);
    assert!(ok, "{said}");
    let source = lab.home.join(".agent/worktrees/demo.fix");
    std::fs::write(source.join("src/lib.rs"), "three\n").unwrap();
    snapshot(&lab, &source, 12, 77);
    let listed = serde_json::json!([{"name": "demo.fix", "bot_id": 5,
        "workspace": source.join("src")}]);
    std::fs::write(lab.scratch.join("run.log.ls"), listed.to_string()).unwrap();
    // A lead's folder that holds repositories and is in none.
    let workspace = lab.scratch.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let (ok, said) = lab.tool(
        &workspace,
        "start_task",
        serde_json::json!({"name": "try", "brief": "b", "from": "fix"}),
        false,
    );
    assert!(ok, "{said}");
    let copy = lab.home.join(".agent/worktrees/demo.try");
    assert_eq!(
        std::fs::read_to_string(copy.join("src/lib.rs")).unwrap(),
        "three\n"
    );
    let ran = lab.ran();
    assert_eq!(ran[5], copy.join("src").to_string_lossy());
    // A worktree of the source's repository, on the fork's branch.
    assert_eq!(
        lab.git(&copy, &["branch", "--show-current"]),
        "agent/demo.try"
    );
    let listed = lab.git(&lab.repo, &["worktree", "list", "--porcelain"]);
    assert!(listed.contains("demo.try"), "{listed}");
}

#[test]
fn a_fork_whose_files_lead_out_of_its_worktree_starts_nothing() {
    let lab = Lab::new("fork-out");
    let (ok, said) = lab.task(false, false);
    assert!(ok, "{said}");
    // A task's files, as any task can write them, make the folder it
    // works in a link to the home, then a folder again.
    let source = lab.home.join(".agent/worktrees/demo.fix");
    std::fs::rename(source.join("src"), source.join("kept")).unwrap();
    std::os::unix::fs::symlink(&lab.home, source.join("src")).unwrap();
    snapshot(&lab, &source, 12, 77);
    std::fs::remove_file(source.join("src")).unwrap();
    std::fs::rename(source.join("kept"), source.join("src")).unwrap();
    let listed = serde_json::json!([{"name": "demo.fix", "bot_id": 5,
        "workspace": source.join("src")}]);
    std::fs::write(lab.scratch.join("run.log.ls"), listed.to_string()).unwrap();
    let (ok, err) = lab.tool(
        &lab.repo.join("src"),
        "start_task",
        serde_json::json!({"name": "try", "brief": "b", "from": "fix"}),
        false,
    );
    assert!(
        !ok && err.contains("the task's folder in its worktree"),
        "{err}"
    );
    assert!(!lab.home.join(".agent/worktrees/demo.try").exists());
    assert!(!lab.scratch.join("run.log.fork").exists());
}

#[test]
fn a_fork_whose_snapshot_names_a_folder_above_its_worktree_starts_nothing() {
    let lab = Lab::new("fork-up");
    let (ok, said) = lab.task(false, false);
    assert!(ok, "{said}");
    let source = lab.home.join(".agent/worktrees/demo.fix");
    snapshot_in(&lab, &source, 12, 77, Some("../.."));
    let listed = serde_json::json!([{"name": "demo.fix", "bot_id": 5,
        "workspace": source.join("src")}]);
    std::fs::write(lab.scratch.join("run.log.ls"), listed.to_string()).unwrap();
    let (ok, err) = lab.tool(
        &lab.repo.join("src"),
        "start_task",
        serde_json::json!({"name": "try", "brief": "b", "from": "fix"}),
        false,
    );
    assert!(!ok && err.contains("not a folder inside it"), "{err}");
    assert!(!lab.home.join(".agent/worktrees/demo.try").exists());
    assert!(!lab.scratch.join("run.log.fork").exists());
}

#[test]
fn a_fork_from_a_turn_that_emptied_its_folder_starts_in_that_folder_made_anew() {
    let lab = Lab::new("fork-empty");
    let (ok, said) = lab.task(false, false);
    assert!(ok, "{said}");
    let source = lab.home.join(".agent/worktrees/demo.fix");
    std::fs::remove_dir_all(source.join("src")).unwrap();
    std::fs::create_dir(source.join("src")).unwrap();
    snapshot(&lab, &source, 12, 77);
    let listed = serde_json::json!([{"name": "demo.fix", "bot_id": 5,
        "workspace": source.join("src")}]);
    std::fs::write(lab.scratch.join("run.log.ls"), listed.to_string()).unwrap();
    let (ok, said) = lab.tool(
        &lab.repo.join("src"),
        "start_task",
        serde_json::json!({"name": "try", "brief": "b", "from": "fix"}),
        false,
    );
    assert!(ok, "{said}");
    let copy = lab.home.join(".agent/worktrees/demo.try");
    assert!(copy.join("src").is_dir());
    assert_eq!(lab.ran()[5], copy.join("src").to_string_lossy());
}

#[test]
fn a_task_forked_from_another_s_turn_gets_its_files_then_in_a_worktree_of_its_own() {
    let lab = Lab::new("fork");
    let (ok, said) = lab.task(false, false);
    assert!(ok, "{said}");
    let source = lab.home.join(".agent/worktrees/demo.fix");
    std::fs::write(source.join("src/lib.rs"), "three\n").unwrap();
    std::fs::write(source.join("src/new.rs"), "made\n").unwrap();
    snapshot(&lab, &source, 12, 77);
    // Work after the turn is not in the fork.
    std::fs::write(source.join("src/lib.rs"), "four\n").unwrap();
    let listed = serde_json::json!([{"name": "demo.fix", "bot_id": 5,
        "workspace": source.join("src")}]);
    std::fs::write(lab.scratch.join("run.log.ls"), listed.to_string()).unwrap();

    let start =
        |call: serde_json::Value| lab.tool(&lab.repo.join("src"), "start_task", call, false);
    let (ok, err) = start(
        serde_json::json!({"name": "other", "brief": "b", "from": "fix",
        "model": "openai/x"}),
    );
    assert!(!ok && err.contains("keeps its source's role"), "{err}");
    let (ok, err) =
        start(serde_json::json!({"name": "other", "brief": "b", "from": "fix", "turn": 11}));
    assert!(!ok && err.contains("turn 11 has no snapshot"), "{err}");
    assert!(!lab.home.join(".agent/worktrees/demo.other").exists());

    // Setup, which runs outside any sandbox, never runs on a task's files.
    let setup = lab.repo.join("src/.agents/setup");
    std::fs::create_dir_all(setup.parent().unwrap()).unwrap();
    std::fs::write(&setup, "#!/bin/sh\ntouch ran\n").unwrap();
    std::fs::set_permissions(&setup, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (ok, said) = start(
        serde_json::json!({"name": "try", "brief": "try another way",
        "from": "fix", "setup": true}),
    );
    assert!(ok, "{said}");
    assert!(
        said.contains("demo.fix's files as they were after its turn 12"),
        "{said}"
    );
    assert!(said.contains(".agents/setup did not run"), "{said}");
    let copy = lab.home.join(".agent/worktrees/demo.try");
    assert!(!copy.join("src/ran").exists());
    let workspace = copy.join("src").to_string_lossy().into_owned();
    let forked = std::fs::read_to_string(lab.scratch.join("run.log.fork")).unwrap();
    let forked: Vec<&str> = forked.lines().collect();
    assert_eq!(
        forked,
        [
            "fork",
            "--source",
            "demo.fix",
            "--bot",
            "demo.try",
            "--checkpoint",
            "77",
            "--workspace",
            &workspace
        ]
    );
    // Only the brief: the fork keeps its source's instructions.
    let ran = lab.ran();
    assert_eq!(
        ran[..6],
        [
            "run",
            "--detach",
            "--bot",
            "demo.try",
            "--workspace",
            &workspace
        ]
    );
    assert_eq!(ran[6], "--");
    assert!(ran[7].contains(&format!("in '{workspace}'")), "{}", ran[7]);
    assert_eq!(ran[8], "try another way");
    let read = |name: &str| std::fs::read_to_string(copy.join(name)).unwrap();
    assert_eq!(read("src/lib.rs"), "three\n");
    assert_eq!(read("src/new.rs"), "made\n");
    assert_eq!(
        lab.git(&copy, &["branch", "--show-current"]),
        "agent/demo.try"
    );
    assert_eq!(lab.git(&copy, &["diff", "--cached", "--name-only"]), "");

    // Removing the source drops its snapshots.
    let (ok, err) = lab.remove(&lab.repo, true);
    assert!(ok, "{err}");
    assert_eq!(
        lab.git(&lab.repo, &["for-each-ref", "refs/agent/snapshots/"]),
        ""
    );
}

#[test]
fn a_task_outside_git_works_in_an_empty_folder_of_its_own() {
    let lab = Lab::new("own-folder");
    // A workspace that is no repository, holding one.
    let ws = lab.scratch.join("ws");
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::write(ws.join("AGENTS.md"), "Build with the workspace tool.\n").unwrap();
    std::fs::rename(&lab.repo, ws.join("src/api")).unwrap();
    // By default, a task shares the folder, and is told so.
    let shared = serde_json::json!({"name": "edit", "brief": "edit the deck"});
    let (ok, said) = lab.tool(&ws, "start_task", shared, false);
    assert!(ok, "{said}");
    assert!(
        said.contains("works in this folder itself, shared"),
        "{said}"
    );
    assert_eq!(
        lab.ran()[6],
        std::fs::canonicalize(&ws).unwrap().to_string_lossy()
    );
    let start = serde_json::json!({"name": "fix", "brief": "fix it", "own_folder": true});
    let (ok, said) = lab.tool(&ws, "start_task", start.clone(), false);
    assert!(ok, "{said}");
    assert!(said.contains("an empty folder of its own"), "{said}");
    let folder = lab.home.join(".agent/worktrees/demo.fix");
    assert_eq!(std::fs::read_dir(&folder).unwrap().count(), 0);
    let ran = lab.ran();
    let after = |flag: &str| {
        let at = ran.iter().position(|arg| arg == flag).unwrap();
        ran[at + 1].clone()
    };
    assert_eq!(after("--workspace"), folder.to_string_lossy());
    // Told the rules of the lead's folder, not of its empty one.
    assert!(ran.join("\n").contains("Build with the workspace tool."));
    assert!(!ran.iter().any(|arg| arg == "--writable"), "{ran:?}");
    // The lead's folder is as it was.
    assert!(!ws.join(".git").exists());
    // Another folder's task of this name does not take it over.
    let other = lab.scratch.join("other");
    std::fs::create_dir_all(&other).unwrap();
    let (ok, said) = lab.tool(&other, "start_task", start.clone(), false);
    assert!(!ok && said.contains("pick another name"), "{said}");
    // One started again from this folder keeps it.
    let (ok, said) = lab.tool(&ws, "start_task", start, false);
    assert!(ok, "{said}");
}

#[test]
fn a_task_s_own_folder_is_removed_only_with_discard() {
    let lab = Lab::new("rm-folder");
    let ws = lab.scratch.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let start = serde_json::json!({"name": "fix", "brief": "fix it", "own_folder": true});
    let (ok, said) = lab.tool(&ws, "start_task", start.clone(), false);
    assert!(ok, "{said}");
    let folder = lab.home.join(".agent/worktrees/demo.fix");
    // The task makes a repository there, at its top, whose settings name
    // a program git would run: Sonny runs nothing in it, and asks for
    // discard.
    let repo = folder.clone();
    lab.git(&repo, &["init", "-q", "-b", "main"]);
    let ran = lab.scratch.join("fsmonitor-ran");
    let hook = lab.scratch.join("fsmonitor");
    std::fs::write(&hook, format!("#!/bin/sh\ntouch {}\n", ran.display())).unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    lab.git(&repo, &["config", "core.fsmonitor", hook.to_str().unwrap()]);
    std::fs::write(lab.scratch.join("run.log.ls"), r#"[{"name":"demo.fix"}]"#).unwrap();
    let (ok, err) = lab.remove(&ws, false);
    assert!(!ok && err.contains("with discard"), "{err}");
    assert!(folder.exists() && !lab.scratch.join("run.log.rm").exists());
    // Removed from another folder, it is refused, and a task of that name
    // started there is not given it.
    let (ok, err) = lab.remove(&lab.repo, true);
    assert!(!ok && err.contains("run this there"), "{err}");
    let (ok, err) = lab.tool(
        &lab.repo,
        "start_task",
        serde_json::json!({"name": "fix", "brief": "x"}),
        false,
    );
    assert!(!ok && err.contains("pick another name"), "{err}");
    let (ok, err) = lab.remove(&ws, true);
    assert!(ok, "{err}");
    assert!(!folder.exists());
    assert!(!lab.home.join(".agent/worktrees/.folders/demo.fix").exists());
    assert!(!ran.exists());
}

#[test]
fn a_task_given_a_repository_in_the_lead_s_folder_gets_a_worktree_of_it() {
    let lab = Lab::new("nested");
    let ws = lab.scratch.join("ws");
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::write(ws.join("AGENTS.md"), "Build with the workspace tool.\n").unwrap();
    let api = ws.join("src/api");
    std::fs::rename(&lab.repo, &api).unwrap();
    // Setup is the lead's folder's, never one the repository holds, which
    // the lead's sandbox does not keep it from writing.
    let planted = lab.scratch.join("planted");
    for (dir, script) in [
        (
            ws.clone(),
            "#!/bin/sh\necho \"$AGENT_SOURCE\" > from\n".to_owned(),
        ),
        (
            api.clone(),
            format!("#!/bin/sh\ntouch {}\n", planted.display()),
        ),
    ] {
        let setup = dir.join(".agents/setup");
        std::fs::create_dir_all(setup.parent().unwrap()).unwrap();
        std::fs::write(&setup, script).unwrap();
        std::fs::set_permissions(&setup, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let start = serde_json::json!({"name": "fix", "brief": format!("fix {}/src/lib.rs", api.display()),
        "folder": "src/api", "setup": true});
    let (ok, said) = lab.tool(&ws, "start_task", start, false);
    assert!(ok, "{said}");
    let worktree = lab.home.join(".agent/worktrees/demo.fix");
    assert!(said.contains("a worktree at"), "{said}");
    let from = std::fs::read_to_string(worktree.join("from")).unwrap();
    assert_eq!(
        from.trim(),
        std::fs::canonicalize(&api).unwrap().to_string_lossy()
    );
    assert!(!planted.exists());
    std::fs::remove_file(worktree.join("from")).unwrap();
    let ran = lab.ran();
    assert_eq!(ran[6], worktree.to_string_lossy());
    assert_eq!(
        lab.git(&worktree, &["branch", "--show-current"]),
        "agent/demo.fix"
    );
    // Told the workspace's rules above the repository too, and the brief
    // names the copy.
    assert!(ran.join("\n").contains("Build with the workspace tool."));
    assert_eq!(
        ran.last().unwrap(),
        &format!("fix {}/src/lib.rs", worktree.display())
    );
    // A role is the lead's project's, found from its folder, not the
    // repository's.
    std::fs::create_dir_all(ws.join(".agents/agents")).unwrap();
    std::fs::write(
        ws.join(".agents/agents/worker.md"),
        "Work as the workspace's worker.\n",
    )
    .unwrap();
    let role = serde_json::json!({"name": "role", "brief": "b", "folder": "src/api",
        "profile": "worker"});
    let (ok, said) = lab.tool(&ws, "start_task", role, false);
    assert!(ok, "{said}");
    let told = lab.ran().join("\n");
    assert!(told.contains("Work as the workspace's worker."), "{told}");
    assert!(told.contains("Build with the workspace tool."), "{told}");
    let (ok, err) = lab.tool(
        &ws,
        "remove_task",
        serde_json::json!({"task": "demo.role", "discard": true}),
        false,
    );
    assert!(ok, "{err}");
    // A folder outside the lead's is refused.
    let away = serde_json::json!({"name": "away", "brief": "b", "folder": "../"});
    let (ok, said) = lab.tool(&ws, "start_task", away, false);
    assert!(!ok && said.contains("not a folder inside"), "{said}");
    // As is one in no repository, or in what git keeps.
    for folder in ["src", "src/api/.git/hooks"] {
        let away = serde_json::json!({"name": "away", "brief": "b", "folder": folder});
        let (ok, said) = lab.tool(&ws, "start_task", away, false);
        assert!(
            !ok && said.contains("not in a git repository"),
            "{folder}: {said}"
        );
    }
    assert!(!lab.home.join(".agent/worktrees/demo.away").exists());
    // As is one the last commit has as a link out of the repository,
    // whatever is there now.
    std::os::unix::fs::symlink(&lab.scratch, api.join("out")).unwrap();
    lab.git(&api, &["add", "out"]);
    lab.git(&api, &["commit", "-qm", "out"]);
    std::fs::remove_file(api.join("out")).unwrap();
    std::fs::create_dir(api.join("out")).unwrap();
    let away = serde_json::json!({"name": "away", "brief": "b", "folder": "src/api/out"});
    let (ok, said) = lab.tool(&ws, "start_task", away, false);
    assert!(!ok && said.contains("leading out of it"), "{said}");
    assert!(!lab.home.join(".agent/worktrees/demo.away").exists());
    assert_eq!(lab.git(&api, &["branch", "--list", "agent/demo.away"]), "");
    // Recorded as the lead's, for its project's memory.
    let record = lab.home.join(".agent/worktrees/.folders/demo.fix");
    assert_eq!(
        std::fs::read_to_string(&record).unwrap().lines().next(),
        Some(&*std::fs::canonicalize(&ws).unwrap().to_string_lossy())
    );
    // Removed from the lead's folder, with its merged branch.
    let (ok, err) = lab.remove(&ws, false);
    assert!(ok, "{err}");
    assert!(!worktree.exists() && !record.exists());
    assert_eq!(lab.git(&api, &["branch", "--list", "agent/demo.fix"]), "");
}

#[test]
fn a_task_given_a_linked_checkout_in_the_lead_s_folder_is_removed_from_it() {
    let lab = Lab::new("linked");
    let ws = lab.scratch.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    // A checkout of a repository whose git folder is outside the lead's
    // folder is refused: the task would write that folder.
    let away = ws.join("away");
    let target = away.to_string_lossy().into_owned();
    lab.git(&lab.repo, &["worktree", "add", "-q", "-b", "away", &target]);
    let start = serde_json::json!({"name": "fix", "brief": "fix it", "folder": "away"});
    let (ok, said) = lab.tool(&ws, "start_task", start, false);
    assert!(!ok && said.contains("outside this folder"), "{said}");
    let worktree = lab.home.join(".agent/worktrees/demo.fix");
    assert!(!worktree.exists());
    assert_eq!(
        lab.git(&lab.repo, &["branch", "--list", "agent/demo.fix"]),
        ""
    );
    // One whose repository is inside it is started, and removed from the
    // checkout it was made of.
    let main = ws.join("main");
    std::fs::rename(&lab.repo, &main).unwrap();
    lab.git(&main, &["worktree", "repair"]);
    let web = ws.join("web");
    let target = web.to_string_lossy().into_owned();
    lab.git(&main, &["worktree", "add", "-q", "-b", "web", &target]);
    let start = serde_json::json!({"name": "fix", "brief": "fix it", "folder": "web"});
    let (ok, said) = lab.tool(&ws, "start_task", start, false);
    assert!(ok, "{said}");
    assert!(worktree.join(".git").exists());
    let (ok, err) = lab.remove(&ws, false);
    assert!(ok, "{err}");
    assert!(!worktree.exists());
    assert_eq!(lab.git(&web, &["branch", "--list", "agent/demo.fix"]), "");
}

#[test]
fn a_task_of_a_lead_in_a_submodule_is_removed_with_its_worktree() {
    let lab = Lab::new("submodule");
    let main = lab.scratch.join("main");
    std::fs::create_dir_all(&main).unwrap();
    lab.git(&main, &["init", "-q", "-b", "main"]);
    let repo = lab.repo.to_string_lossy().into_owned();
    lab.git(
        &main,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            &repo,
            "sub",
        ],
    );
    let sub = main.join("sub");
    let start = serde_json::json!({"name": "fix", "brief": "fix it"});
    let (ok, said) = lab.tool(&sub, "start_task", start, false);
    assert!(ok, "{said}");
    let worktree = lab.home.join(".agent/worktrees/demo.fix");
    assert!(worktree.join(".git").exists());
    let (ok, err) = lab.remove(&sub, false);
    assert!(ok, "{err}");
    assert!(!worktree.exists());
}

#[test]
fn a_fork_of_a_task_in_a_repository_inside_the_lead_s_folder_is_removed_from_the_lead_s() {
    let lab = Lab::new("fork-inside");
    let ws = lab.scratch.join("ws");
    std::fs::create_dir_all(ws.join("src")).unwrap();
    let api = ws.join("src/api");
    std::fs::rename(&lab.repo, &api).unwrap();
    let start = serde_json::json!({"name": "fix", "brief": "b", "folder": "src/api"});
    let (ok, said) = lab.tool(&ws, "start_task", start, false);
    assert!(ok, "{said}");
    let source = lab.home.join(".agent/worktrees/demo.fix");
    std::fs::write(source.join("src/lib.rs"), "three\n").unwrap();
    snapshot(&lab, &source, 12, 77);
    let listed = serde_json::json!([{"name": "demo.fix", "bot_id": 5, "workspace": source}]);
    std::fs::write(lab.scratch.join("run.log.ls"), listed.to_string()).unwrap();
    let fork = serde_json::json!({"name": "try", "brief": "b", "from": "fix"});
    let (ok, said) = lab.tool(&ws, "start_task", fork, false);
    assert!(ok, "{said}");
    let copy = lab.home.join(".agent/worktrees/demo.try");
    assert_eq!(
        std::fs::read_to_string(copy.join("src/lib.rs")).unwrap(),
        "three\n"
    );
    let call = serde_json::json!({"task": "demo.try", "discard": true});
    let (ok, said) = lab.tool(&ws, "remove_task", call, false);
    assert!(ok, "{said}");
    assert!(!copy.exists());
    assert_eq!(lab.git(&api, &["branch", "--list", "agent/demo.try"]), "");
}

#[test]
fn no_fork_comes_back_through_a_sealed_folder_a_bot_s_folder_holds() {
    let lab = Lab::new("fork-housed");
    let (ok, said) = lab.task(false, false);
    assert!(ok, "{said}");
    let source = lab.home.join(".agent/worktrees/demo.fix");
    snapshot(&lab, &source, 12, 77);
    // A bot works in the folder that holds the store and its sealed one.
    let listed = serde_json::json!([
        {"name": "demo.fix", "bot_id": 5, "workspace": source.join("src")},
        {"name": "demo.wide", "bot_id": 6, "workspace": lab.scratch},
    ]);
    std::fs::write(lab.scratch.join("run.log.ls"), listed.to_string()).unwrap();
    let (ok, err) = lab.tool(
        &lab.repo.join("src"),
        "start_task",
        serde_json::json!({"name": "try", "brief": "b", "from": "fix"}),
        false,
    );
    assert!(!ok && err.contains("where a bot works"), "{err}");
    assert!(!lab.home.join(".agent/worktrees/demo.try").exists());
    assert!(!lab.scratch.join("run.log.fork").exists());
}
