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
fn a_task_s_own_folder_is_removed_once_its_repositories_hold_nothing_unsaved() {
    let lab = Lab::new("rm-folder");
    let ws = lab.scratch.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let start = serde_json::json!({"name": "fix", "brief": "fix it", "own_folder": true});
    let (ok, said) = lab.tool(&ws, "start_task", start, false);
    assert!(ok, "{said}");
    let folder = lab.home.join(".agent/worktrees/demo.fix");
    // The task makes a workspace: a clone of the repository, as a
    // workspace's tooling would, and one nested in it with no remote.
    let api = folder.join("ws2/src/api");
    let (ok, _, err) = lab.run(
        &lab.scratch,
        "git",
        &[
            "clone",
            "-q",
            lab.repo.to_str().unwrap(),
            api.to_str().unwrap(),
        ],
        false,
    );
    assert!(ok, "{err}");
    std::fs::write(api.join(".git/info/exclude"), "/deps/\n").unwrap();
    let deps = api.join("deps/lib");
    std::fs::create_dir_all(&deps).unwrap();
    lab.git(&deps, &["init", "-q", "-b", "main"]);
    std::fs::write(folder.join("ws2/build.log"), "x\n").unwrap();
    // Uncommitted work in the nested repository keeps it.
    std::fs::write(deps.join("new.txt"), "x\n").unwrap();
    let (ok, err) = lab.remove(&ws, false);
    assert!(!ok && err.contains("ws2/src/api/deps/lib"), "{err}");
    assert!(!err.contains("ws2/src/api,"), "{err}");
    // So do commits no remote has.
    lab.git(&deps, &["add", "."]);
    lab.git(&deps, &["commit", "-qm", "dep"]);
    lab.git(&api, &["commit", "-q", "--allow-empty", "-m", "api"]);
    let (ok, err) = lab.remove(&ws, false);
    assert!(
        !ok && err.contains("not committed or not pushed in ws2/src/api, ws2/src/api/deps/lib"),
        "{err}"
    );
    assert!(folder.exists());
    // Pushed, the folder goes with all it holds, but not while a detached
    // checkout holds a commit no remote has.
    lab.git(&api, &["push", "-q", "origin", "HEAD:refs/heads/done"]);
    std::fs::remove_dir_all(&deps).unwrap();
    lab.git(&api, &["checkout", "-q", "--detach"]);
    lab.git(&api, &["commit", "-q", "--allow-empty", "-m", "detached"]);
    let (ok, err) = lab.remove(&ws, false);
    assert!(!ok && err.contains("ws2/src/api"), "{err}");
    lab.git(&api, &["push", "-q", "origin", "HEAD:refs/heads/detached"]);
    let (ok, err) = lab.remove(&ws, false);
    assert!(ok, "{err}");
    assert!(!folder.exists());
    assert!(!lab.home.join(".agent/worktrees/.folders/demo.fix").exists());
    // Removed from another folder, a task's own folder is refused.
    let (ok, said) = lab.tool(
        &ws,
        "start_task",
        serde_json::json!({"name": "fix", "brief": "b", "own_folder": true}),
        false,
    );
    assert!(ok, "{said}");
    let (ok, err) = lab.remove(&lab.repo, false);
    assert!(!ok && err.contains("run this there"), "{err}");
    let (ok, err) = lab.remove(&ws, true);
    assert!(ok, "{err}");
}

#[test]
fn a_task_given_a_repository_in_the_lead_s_folder_gets_a_worktree_of_it() {
    let lab = Lab::new("nested");
    let ws = lab.scratch.join("ws");
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::write(ws.join("AGENTS.md"), "Build with the workspace tool.\n").unwrap();
    let api = ws.join("src/api");
    std::fs::rename(&lab.repo, &api).unwrap();
    let start = serde_json::json!({"name": "fix", "brief": format!("fix {}/src/lib.rs", api.display()),
        "folder": "src/api"});
    let (ok, said) = lab.tool(&ws, "start_task", start, false);
    assert!(ok, "{said}");
    let worktree = lab.home.join(".agent/worktrees/demo.fix");
    assert!(said.contains("a worktree at"), "{said}");
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
    // A folder outside the lead's is refused.
    let away = serde_json::json!({"name": "away", "brief": "b", "folder": "../"});
    let (ok, said) = lab.tool(&ws, "start_task", away, false);
    assert!(!ok && said.contains("not a folder inside"), "{said}");
    // Removed from the lead's folder, with its merged branch.
    let (ok, err) = lab.remove(&ws, false);
    assert!(ok, "{err}");
    assert!(!worktree.exists());
    assert_eq!(lab.git(&api, &["branch", "--list", "agent/demo.fix"]), "");
}
