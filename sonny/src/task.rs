//! A lead's tasks, each in a folder of its own: the `start_task` tool Sonny
//! answers makes the task a git worktree, then starts it there with the
//! daemon's `agent run`. A worktree starts at the last commit on a branch
//! of its own, with clones of the lead's build folders where the disk makes
//! them copy-on-write (seed.rs); outside git, the task shares the lead's
//! folder. With `own_folder`, the task gets an empty folder of its own
//! instead and makes there what it works in, as the person's tooling makes
//! a workspace, reading the lead's folder where it is. A task that could
//! not start leaves no folder behind, and the `remove_task` tool takes away
//! one that is done.
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The task's folder, how its work comes back, and what undoes it.
struct Place {
    dir: PathBuf,
    /// The folder whose rules, roles and memory the task is told: the
    /// lead's, or its copy.
    told: PathBuf,
    note: Option<String>,
    undo: Undo,
    /// The lead's folder the task's is a copy of, and the copy's top: a
    /// brief that names a path in the one names it in the other.
    copy: Option<(PathBuf, PathBuf)>,
}

/// What a start that failed takes away: only what it made.
enum Undo {
    Nothing,
    Worktree {
        here: PathBuf,
        path: PathBuf,
        branch: String,
    },
    /// An empty folder of the task's own, and Sonny's record of it.
    Folder {
        path: PathBuf,
        record: PathBuf,
    },
}

/// A `start_task` call.
pub struct Start {
    pub name: String,
    pub brief: String,
    pub profile: Option<String>,
    pub setup: bool,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// An empty folder of the task's own, rather than a worktree or, outside
    /// git, the lead's folder itself.
    pub own_folder: bool,
    /// A folder inside the lead's that the task works at instead of the
    /// lead's own, such as a repository it holds, of which the task then
    /// gets a worktree.
    pub folder: Option<String>,
    /// A task to fork, with its files as they were after its turn `turn`,
    /// else its newest that has them.
    pub from: Option<String>,
    pub turn: Option<i64>,
    /// The task starting this one as its own helper, which builds on its
    /// work: in the repository it works in, unless `folder` or
    /// `own_folder` says otherwise, from the last commit on its branch.
    pub builds_on: Option<String>,
}

/// The task a fork copies, and its files after the turn it copies.
struct Source {
    name: String,
    /// The top of its worktree, which its snapshot holds, and where in
    /// it the task worked.
    top: PathBuf,
    prefix: PathBuf,
    snapshot: crate::snapshot::Snapshot,
}

/// Start the task `start` asks for in `project`, whose lead's folder is
/// `here`, for `starter`, the lead or a task of its line, whose mode it
/// works in, and say what started and how its work comes back.
pub fn start(here: &Path, project: &str, starter: &str, start: &Start) -> Result<String, String> {
    let short = start.name.trim();
    // A lead's name is never a task's.
    if short == "lead" || short.starts_with("lead.") || crate::project::is_lead(short) {
        return Err(format!(
            "{short:?} names a lead; give the task a name of its own"
        ));
    }
    let name = match short.strip_prefix(&format!("{project}.")) {
        Some(_) => short.to_owned(),
        None => format!("{project}.{short}"),
    };
    let name = name.as_str();
    if short.is_empty() || !branch_name(name) {
        return Err(format!("{short:?} is not a name a git branch can have"));
    }
    if start.brief.trim().is_empty() {
        return Err("a task starts with a brief".into());
    }
    // The daemon's binary in an agent's shell; in a terminal, the one
    // Sonny would start.
    let agent = crate::agent_bin().ok_or("Agent's binary was not found; set AGENT_BIN")?;
    // With the provider keys the app keeps, should this start the daemon;
    // read before the folder is made, so a bad env file leaves nothing.
    let mut command = crate::agent_command(&agent)?;
    let config = agent_client::config::load()?;

    let info = agent_client::project::read(here)?;
    let mut flags = match &start.profile {
        Some(role) => vec!["--profile".to_owned(), role.clone()],
        None => vec!["--agents".to_owned()],
    };
    if let Some(model) = start.model.clone().filter(|m| !m.is_empty()) {
        flags.extend(["--model".to_owned(), model]);
    }
    if let Some(effort) = start.effort.clone().filter(|e| !e.is_empty()) {
        flags.extend(["--effort".to_owned(), effort]);
    }
    // A helper works in the repository inside the lead's folder that its
    // task works in, unless told another; a fork works where its source did.
    let theirs = match (&start.builds_on, start.own_folder) {
        (Some(task), false) if start.folder.is_none() && start.from.is_none() => {
            match recorded(task) {
                Some((source, Made::Worktree(repo))) if same(&source, here) => Some(repo),
                _ => None,
            }
        }
        _ => None,
    };
    let theirs = theirs.map(|repo| repo.to_string_lossy().into_owned());
    let folder = (start.folder.as_deref())
        .or(theirs.as_deref())
        .filter(|f| !f.trim().is_empty());
    if folder.is_some() && (start.own_folder || info["threads_in"] == "project") {
        return Err(match start.own_folder {
            true => "folder and own_folder each give the task its folder; give one".into(),
            false => "this project's tasks all work in this folder; start it without folder".into(),
        });
    }
    // Where in the lead's folder the task works: the lead's own place, or
    // the folder inside it named.
    let at = match folder {
        Some(folder) => {
            let at = inside(here, folder)?;
            // A repository's, which the task gets a worktree of: never
            // the lead's folder by another name, nor a folder git keeps,
            // which a task working in would write past what keeps it.
            if git(&at, &["rev-parse", "--show-toplevel"]).is_err() {
                return Err(format!(
                    "{folder} is not in a git repository; give folder only for one, whose \
                     worktree the task gets"
                ));
            }
            at
        }
        None => here.to_owned(),
    };
    let source = match &start.from {
        Some(_) if folder.is_some() || start.own_folder => {
            return Err(
                "a fork works where its source did; give neither folder nor own_folder".into(),
            );
        }
        Some(from) => Some(source(&agent, project, from, start)?),
        None => None,
    };
    // The worktree of the task a helper builds on, whose files it starts
    // with as they were at its last commit; files a task wrote, as a
    // fork's are.
    let mut helped = None;
    let mut written = source.is_some();
    // The lead's mode, whose sandbox the task works in, and Sonny's git for
    // it in a sandbox too unless the lead has none.
    let mode = lead_mode(&agent, starter).unwrap_or_else(crate::approvals::default);
    let confine = crate::approvals::sandbox(mode, here)
        .get("sandbox")
        .and_then(Value::as_str)
        != Some("danger-full-access");
    let mut place = match &source {
        Some(source) => forked(&agent, name, source)?,
        None if info["threads_in"] == "project" => in_place(here, name),
        None if start.own_folder => own_folder(here, name)?,
        None => {
            // On the last commit of the branch of the task it helps, in
            // the repository the helper is made of, when it has one there.
            let base = (start.builds_on.as_deref()).and_then(|task| branch_of(&at, task));
            if let (Some(task), Some(_)) = (&start.builds_on, &base) {
                helped = worktree_of(&at, task);
                written = true;
            }
            worktree(&at, name, base.as_deref().unwrap_or("HEAD"), confine)?
        }
    };
    // A worktree's files come from commits a task can write, which may
    // make the lead's subfolder a link out of it: the task would then work
    // wherever it leads, with the sandbox's leave to write there. So each
    // folder down to the task's is opened without following a link, and
    // made when the commit has none, as when its last file was removed.
    if let Undo::Worktree { path, .. } = &place.undo {
        let below = place.dir.strip_prefix(path).ok().and_then(|below| {
            (below.components())
                .map(|part| match part {
                    std::path::Component::Normal(part) => Some(part),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()
        });
        let made = match below {
            Some(parts) => crate::snapshot::folders(path, &parts, true).map_err(|e| e.to_string()),
            None => Err("not a folder inside it".to_owned()),
        };
        if let Err(e) = made {
            undo(&place.undo);
            return Err(format!(
                "{}: the task's folder in its worktree {}, leading out of it or no folder: {e}",
                place.dir.display(),
                path.display()
            ));
        }
    }
    // The task's folder in its worktree is the worktree's own, not one a
    // link committed in its place leads out of it to.
    if let Some((_, path)) = &place.copy {
        let tree = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
        if !std::fs::canonicalize(&place.dir).is_ok_and(|dir| dir.starts_with(&tree)) {
            undo(&place.undo);
            return Err(format!(
                "{} is a link in the repository's last commit, leading out of it; start the \
                 task from a folder that is not",
                at.display()
            ));
        }
    }
    // A task given a folder writes the git folder of the repository its
    // worktree was made of, which must be inside the lead's, as the
    // lead's own is: not a repository elsewhere that a link or a `.git`
    // file there names, whenever it was put there.
    if at != here
        && let Some((_, path)) = &place.copy
    {
        let common = git(
            path,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .ok()
        .and_then(|dir| std::fs::canonicalize(dir).ok());
        let base = std::fs::canonicalize(here).unwrap_or_else(|_| here.to_owned());
        if !common.is_some_and(|common| common.starts_with(&base)) {
            undo(&place.undo);
            return Err(format!(
                "the repository at {} keeps its git folder outside this folder; start the \
                 task from a folder holding it",
                at.display()
            ));
        }
    }
    // A task in a folder inside the lead's is told the rules of the
    // folders above it too, which its worktree is not inside.
    // Recorded as the lead's, like an own folder, so its memory is the
    // lead's project's, whatever project the repository's folder is in,
    // with the checkout it was made from, which removes it.
    let mut recorded = None;
    if at != here {
        place.told = at.clone();
        if let (Some((top, _)), Ok(record)) = (&place.copy, record_of(name))
            && !record.exists()
        {
            let text = format!("{}\n{}\n", here.display(), top.display());
            let wrote = std::fs::create_dir_all(record.parent().unwrap_or(Path::new("/")))
                .and_then(|()| std::fs::write(&record, text));
            if let Err(e) = wrote {
                undo(&place.undo);
                return Err(format!("{}: {e}", record.display()));
            }
            recorded = Some(record);
        }
    }
    // A fork of a task Sonny recorded, made in the same repository, is
    // recorded as it is, so the lead that started it removes it there.
    if let Some(source) = &source
        && let Some((_, Made::Worktree(_))) = self::recorded(&source.name)
        && let (Some((top, _)), Ok(record)) = (&place.copy, record_of(name))
    {
        let text = format!("{}\n{}\n", here.display(), top.display());
        let wrote = std::fs::create_dir_all(record.parent().unwrap_or(Path::new("/")))
            .and_then(|()| std::fs::write(&record, text));
        if let Err(e) = wrote {
            undo(&place.undo);
            return Err(format!("{}: {e}", record.display()));
        }
        recorded = Some(record);
    }
    // Only a run that started can have made the task, which then keeps
    // its folder; anything that failed before leaves nothing behind.
    let failed = |place: &Place, ran: bool, error: String| {
        if !ran || !listed(&agent, name) {
            undo(&place.undo);
            if let Some(record) = &recorded {
                let _ = std::fs::remove_file(record);
            }
        }
        Err(error)
    };
    // The lead's folder's setup script, run in the task's own folder, as
    // for a worktree; a task in the lead's folder has nothing to set up.
    // Never a repository's inside it: its `.agents`, unlike the lead's, is
    // no more kept from the lead's sandbox than the rest of it, and the
    // script runs outside any.
    // A fork's files, and a helper's on its task's branch, are what a
    // task wrote, so setup never runs on them: the agent runs it in its
    // own.
    let script = here.join(".agents/setup");
    if start.setup
        && written
        && script.exists()
        && let Some(note) = place.note.as_mut()
    {
        note.push_str(
            "; .agents/setup did not run on these files, which a task wrote: the task can \
             run it there if the work needs it",
        );
    }
    if start.setup && !written && place.dir != at && script.exists() {
        // Its output is not kept, however long a build runs: only the last
        // lines of its errors, which a failure shows.
        let mut setup = Command::new(&script);
        // The folder's own code, run outside any sandbox: none of the
        // keys Agent keeps, as a bot's shell has none.
        for key in kept_keys() {
            setup.env_remove(key);
        }
        let ran = setup
            .current_dir(&place.dir)
            .env("AGENT_SOURCE", &at)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                let tail = child.stderr.take().map(tail).unwrap_or_default();
                Ok((child.wait()?, tail))
            });
        match ran {
            Ok((status, _)) if status.success() => {}
            Ok((_, tail)) => {
                let error = format!("{} failed:\n{}", script.display(), tail.join("\n"));
                return failed(&place, false, error);
            }
            Err(e) => return failed(&place, false, format!("{}: {e}", script.display())),
        }
    }
    // A new worktree takes clones of the lead's build folders only once
    // setup has run: setup runs outside any sandbox and must not run what
    // the lead's folders hold. A fork or a task's helper takes none: Sonny
    // writes nothing of its own among files a task wrote, whose links
    // could lead it out.
    if let (Undo::Worktree { path, .. }, Some((top, _)), false) =
        (&place.undo, &place.copy, written)
    {
        let seeded = crate::seed::seed(top, path);
        // Named so the person knows why the first build is quicker.
        if let (false, Some(note)) = (seeded.is_empty(), place.note.as_mut()) {
            let folders: Vec<_> = seeded.iter().map(|f| f.to_string_lossy()).collect();
            note.push_str(&format!(
                "; it starts with copy-on-write clones of {}",
                folders.join(", ")
            ));
        }
    }
    let lead_picks = (
        std::env::var("AGENT_MODEL").ok(),
        std::env::var("AGENT_EFFORT").ok(),
    );
    let flags: Vec<&String> = flags.iter().collect();
    let flags = with_defaults(&info, &flags, lead_picks, config.sonny.task_effort);
    let flags: Vec<&String> = flags.iter().collect();
    // Roles are the lead's project's, which `info` names, even for a task
    // told the rules of a folder inside it.
    let roles = if at != here { here } else { &place.told };
    // A fork's instructions are its source's, kept by the daemon; a role
    // named now may not be in the files it gets back.
    let composed = match &source {
        Some(_) => Ok(Vec::new()),
        None => told(&place.told, roles, here, &info, &flags),
    };
    let mut flags = match composed {
        Ok(flags) => flags,
        Err(error) => return failed(&place, false, error),
    };
    with_cache(&mut flags);
    // A task works in its lead's mode's sandbox: confined to its folder,
    // committing to its repository and running the commands the config
    // lets out of it, or, in `full`, unconfined, as nothing would answer a
    // call that asks to leave.
    for (key, value) in crate::approvals::sandbox(mode, &place.dir) {
        let flag = format!("--{key}");
        match value {
            Value::String(value) => flags.extend([flag, value]),
            Value::Bool(true) => flags.push(flag),
            Value::Array(values) => {
                for value in values.iter().filter_map(Value::as_str) {
                    flags.extend([flag.clone(), value.to_owned()]);
                }
            }
            _ => {}
        }
    }
    // A path in the lead's folder would have the task write there instead
    // of in its copy, where nothing keeps it apart from the lead's work;
    // so would one in the folder of a task it forks.
    let mut brief = vec![start.brief.clone()];
    if let Some(source) = &source {
        brief.insert(
            0,
            format!(
                "You are a copy of {} as it was after its turn {}, now {name}, in {}: a \
                 worktree of its own with the files as they were then. Work only there.",
                source.name,
                source.snapshot.turn,
                shell_quote(&place.dir)
            ),
        );
        // Its files are where they were in its worktree, from the top.
        if let Some((_, worktree)) = &place.copy {
            brief = in_copy(&brief, &source.top, worktree).0;
        }
    }
    // A helper's are where they were in its task's worktree.
    if let (Some(top), Some((_, worktree))) = (&helped, &place.copy) {
        brief = in_copy(&brief, top, worktree).0;
    }
    let (brief, moved) = match &place.copy {
        Some((from, to)) => in_copy(&brief, from, to),
        None => (brief, 0),
    };
    if let Some(source) = &source {
        // A fork keeps its source's instructions, role, model and sandbox.
        let forked = crate::agent_command(&agent)?
            .args([
                "fork",
                "--source",
                &source.name,
                "--bot",
                name,
                "--checkpoint",
            ])
            .arg(source.snapshot.checkpoint.to_string())
            .arg("--workspace")
            .arg(&place.dir)
            .stdin(Stdio::null())
            .output();
        match forked {
            Ok(output) if output.status.success() => {}
            Ok(output) => {
                let said = String::from_utf8_lossy(&output.stderr);
                return failed(
                    &place,
                    true,
                    format!("{name} was not forked: {}", said.trim()),
                );
            }
            Err(e) => {
                return failed(&place, false, format!("running {}: {e}", agent.display()));
            }
        }
        flags.clear();
    }
    command.args(["run", "--detach"]);
    if source.is_none() {
        command.arg("--new");
    }
    let output = command
        .args(["--bot", name, "--workspace"])
        .arg(&place.dir)
        .args(flags)
        .arg("--")
        .args(&brief)
        .stdin(Stdio::null())
        .output();
    let output = match output {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            let said = String::from_utf8_lossy(&output.stderr);
            return failed(
                &place,
                true,
                format!("{name} did not start: {}", said.trim()),
            );
        }
        Err(e) => {
            return failed(&place, false, format!("running {}: {e}", agent.display()));
        }
    };
    let mut said = vec![format!(
        "Started {name}: {}",
        String::from_utf8_lossy(&output.stdout).trim()
    )];
    said.extend(place.note.clone());
    if let (Some((from, to)), 1..) = (&place.copy, moved) {
        said.push(format!(
            "The brief named {} {moved} time{}; it names the task's copy {} instead.",
            shell_quote(from),
            if moved == 1 { "" } else { "s" },
            shell_quote(to)
        ));
    }
    Ok(said.join("\n"))
}

/// Whether two paths name one folder, links followed.
fn same(one: &Path, other: &Path) -> bool {
    let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_owned());
    real(one) == real(other)
}

/// The branch Sonny made the task `name` in the repository at `here`, on
/// which a task it starts begins, so it builds on its work; `None` for a
/// task Sonny made none there.
fn branch_of(here: &Path, name: &str) -> Option<String> {
    let branch = format!("agent/{name}");
    git(
        here,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .ok()?;
    Some(branch)
}

/// Where the task `name`'s branch is checked out, in the repository at
/// `here`.
fn worktree_of(here: &Path, name: &str) -> Option<PathBuf> {
    let list = git(here, &["worktree", "list", "--porcelain"]).ok()?;
    let branch = format!("branch refs/heads/agent/{name}");
    (list.split("\n\n"))
        .find(|entry| entry.lines().any(|line| line == branch))
        .and_then(|entry| {
            entry
                .lines()
                .find_map(|line| line.strip_prefix("worktree "))
        })
        .map(PathBuf::from)
}

/// Whether git would take `name` for the branch `agent/NAME`.
fn branch_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('/')
        && Command::new("git")
            .args(["check-ref-format", "--branch", &format!("agent/{name}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
}

/// The run flags with the task's model and effort, unless they name their
/// own: the project's `threads_model`, else the lead's (`lead`, its
/// AGENT_MODEL and AGENT_EFFORT), so a role (--profile) never swaps it;
/// and the project's `threads_effort`, else `task_effort`
/// (the config's `[sonny] task_effort`), else the lead's own when the task
/// runs its model.
fn with_defaults(
    info: &Value,
    flags: &[&String],
    lead: (Option<String>, Option<String>),
    task_effort: Option<String>,
) -> Vec<String> {
    let mut flags: Vec<String> = flags.iter().map(|flag| flag.to_string()).collect();
    // A value flag is --flag VALUE or --flag=VALUE.
    let value = |flags: &[String], name: &str| -> Option<Option<String>> {
        let at = flags.iter().position(|flag| {
            flag == name || flag.strip_prefix(name).is_some_and(|r| r.starts_with('='))
        })?;
        Some(match flags[at].split_once('=') {
            Some((_, value)) => Some(value.to_owned()),
            None => flags.get(at + 1).cloned(),
        })
    };
    let (lead_model, lead_effort) = lead;
    let model = match value(&flags, "--model") {
        Some(model) => model,
        None => {
            let model = info["threads_model"]
                .as_str()
                .map(str::to_owned)
                .or(lead_model.clone());
            if let Some(model) = &model {
                flags.extend(["--model".to_owned(), model.clone()]);
            }
            model
        }
    };
    if value(&flags, "--effort").is_none() {
        let effort = info["threads_effort"]
            .as_str()
            .map(str::to_owned)
            .or(task_effort)
            .or(lead_effort.filter(|_| model.is_some() && model == lead_model));
        if let Some(effort) = effort {
            flags.extend(["--effort".to_owned(), effort]);
        }
    }
    flags
}

/// The run flags with the prompt-cache lifetime Sonny gives the model it
/// names, unless they name their own, as a bot Sonny makes has it.
fn with_cache(flags: &mut Vec<String>) {
    let named = |name: &str| {
        let at = flags.iter().position(|flag| {
            flag == name || flag.strip_prefix(name).is_some_and(|r| r.starts_with('='))
        })?;
        Some(match flags[at].split_once('=') {
            Some((_, value)) => value.to_owned(),
            None => flags.get(at + 1).cloned().unwrap_or_default(),
        })
    };
    if named("--cache-ttl").is_some() {
        return;
    }
    if let Some(ttl) = named("--model").and_then(|model| agent_client::policy::cache_ttl(&model)) {
        flags.extend(["--cache-ttl".to_owned(), ttl.to_owned()]);
    }
}

/// The run flags with what `--agents` or `--profile ROLE` would compose
/// for a task in `dir` given as `--instructions`, with what every task is
/// told (`task.md`, or the task profile the project `info` or the config
/// names) after it, and the role's model and tools unless the flags name
/// their own. Roles are found from `roles`. Other flags pass as they are.
fn told(
    dir: &Path,
    roles: &Path,
    lead: &Path,
    info: &Value,
    flags: &[&String],
) -> Result<Vec<String>, String> {
    let mut kept = Vec::new();
    let (mut agents, mut profile) = (false, None);
    let mut flags = flags.iter().map(|flag| flag.as_str());
    while let Some(flag) = flags.next() {
        match flag {
            "--agents" => agents = true,
            "--profile" => profile = Some(flags.next().ok_or("--profile needs a ROLE")?),
            _ => match flag.strip_prefix("--profile=") {
                Some(role) => profile = Some(role),
                None => kept.push(flag.to_owned()),
            },
        }
    }
    if !agents && profile.is_none() {
        return Ok(kept);
    }
    // A value flag is --flag VALUE or --flag=VALUE.
    let named = |flag: &str| {
        (kept.iter()).any(|k| k.split_once('=').map_or(k.as_str(), |(name, _)| name) == flag)
    };
    if named("--instructions") || named("--instructions-file") {
        return Err("--agents and --profile compose instructions; --instructions sets them".into());
    }
    // What every task is told comes after any role, so its body is never
    // the role's too; its model and tools still apply when it is named.
    let task = crate::project::task_profile(info)?;
    let role = match profile {
        Some(name) => Some(crate::project::profile(roles, name)?),
        None => None,
    };
    // The same profile when it is the same file (a case-insensitive
    // filesystem gives `Worker` and `worker` one), or the same built-in.
    let told_after = |role: &&agent_client::policy::Profile| match &role.path {
        None => role.name == task,
        Some(path) => crate::project::profile(roles, &task).is_ok_and(|task| {
            let real = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
            task.path.is_some_and(|other| real(&other) == real(path))
        }),
    };
    // The memory of the lead's project, wherever the task works.
    let memory = agent_client::policy::memory_indexes(lead).map_err(|e| e.to_string())?;
    let composed =
        agent_client::policy::instructions(dir, role.as_ref().filter(|r| !told_after(r)), &memory)
            .map_err(|e| format!("{}: {e}", e.code()))?;
    let mut added = Vec::new();
    if let Some(role) = &role {
        if let Some(model) = role.model.as_ref().filter(|_| !named("--model")) {
            added.extend(["--model".to_owned(), model.clone()]);
        }
        if let Some(tools) = role.tools.as_ref().filter(|_| !named("--tools")) {
            added.extend(["--tools".to_owned(), tools.join(",")]);
        }
    }
    added.extend([
        "--instructions".to_owned(),
        crate::project::as_task(roles, info, &composed.text)?,
    ]);
    kept.extend(added);
    Ok(kept)
}

/// `brief` with each path in the folder `from` turned into the same path
/// in its copy `to`, and how many were. A folder has more names than its
/// own: the one the lead's shell was given (`$PWD`, through a link), a
/// link in the home to it or a folder holding it (`~/workplace` for
/// `/Volumes/workplace`), each also from `~`, and macOS's `/tmp` for
/// `/private/tmp`.
fn in_copy(brief: &[String], from: &Path, to: &Path) -> (Vec<String>, usize) {
    let names = names(from);
    let to = to.to_string_lossy();
    let mut moved = 0;
    let brief = brief
        .iter()
        .map(|arg| {
            let (arg, count) = replace_paths(arg, &names, &to);
            moved += count;
            arg
        })
        .collect();
    (brief, moved)
}

/// Every name of the folder `from` a brief might use, longest first.
fn names(from: &Path) -> Vec<String> {
    let mut names = vec![from.to_path_buf()];
    let canonical = std::fs::canonicalize(from).unwrap_or_else(|_| from.to_owned());
    names.push(canonical.clone());
    // The shell's own name for where it is, which may go through a link.
    if let (Some(pwd), Ok(here)) = (std::env::var_os("PWD"), std::env::current_dir()) {
        let pwd = PathBuf::from(pwd);
        let here = std::fs::canonicalize(here).unwrap_or_default();
        if std::fs::canonicalize(&pwd).is_ok_and(|real| real == here)
            && let Ok(below) = here.strip_prefix(&canonical)
        {
            let mut top = pwd;
            for _ in below.components() {
                top.pop();
            }
            // Entered through a link to a folder inside it, what is left names
            // something else.
            if std::fs::canonicalize(&top).is_ok_and(|real| real == canonical) {
                names.push(top);
            }
        }
    }
    let home = home_dir().ok();
    // Links in the home to the folder, or to a folder holding it.
    if let Some(entries) = home.as_ref().and_then(|home| std::fs::read_dir(home).ok()) {
        for entry in entries.flatten() {
            let link = entry.path();
            if !entry.file_type().is_ok_and(|kind| kind.is_symlink()) {
                continue;
            }
            if let Ok(target) = std::fs::canonicalize(&link)
                && let Ok(below) = canonical.strip_prefix(&target)
            {
                names.push(within(&link, below));
            }
        }
    }
    let mut names: Vec<String> = names
        .iter()
        .flat_map(|name| {
            let name = name.to_string_lossy().into_owned();
            // macOS's `/tmp` and `/var` are `/private/tmp` and `/private/var`;
            // other folders under `/private` have no such second name.
            let short = name
                .strip_prefix("/private")
                .filter(|rest| rest.starts_with('/'))
                .filter(|rest| {
                    std::fs::canonicalize(rest)
                        .is_ok_and(|real| Some(real) == std::fs::canonicalize(&name).ok())
                });
            [Some(name.clone()), short.map(str::to_owned)]
        })
        .flatten()
        .collect();
    if let Some(home) = home
        .as_ref()
        .map(|home| home.to_string_lossy().into_owned())
    {
        let tilde: Vec<String> = names
            .iter()
            .filter_map(|name| name.strip_prefix(&home))
            .filter(|rest| rest.starts_with('/'))
            .map(|rest| format!("~{rest}"))
            .collect();
        names.extend(tilde);
    }
    names.retain(|name| name.len() > 1);
    names.sort_by_key(|name| std::cmp::Reverse(name.len()));
    names.dedup();
    names
}

/// `text` with each of `names` that stands as a whole path, or as the
/// start of one, turned into `to`; and how many were.
fn replace_paths(text: &str, names: &[String], to: &str) -> (String, usize) {
    // A name inside a longer one (`/work` in `/workplace`, `/work+old` or
    // `/x/work`) is another path. Names end at spaces and at what sets a path
    // apart in prose and commands: quotes, brackets, `=`, `:` and the like.
    let part_of_name = |c: char| {
        !c.is_whitespace()
            && !matches!(
                c,
                '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>'
            )
            && !matches!(c, ',' | ';' | ':' | '=' | '|' | '&' | '!' | '?' | '*')
    };
    let mut out = String::with_capacity(text.len());
    let mut count = 0;
    let mut rest = text;
    'scan: while !rest.is_empty() {
        let at_start = out.chars().next_back().is_none_or(|c| !part_of_name(c));
        if at_start {
            for name in names {
                if let Some(after) = rest.strip_prefix(name.as_str())
                    && ends_name(after, part_of_name)
                    && stays_inside(after, part_of_name)
                {
                    out.push_str(to);
                    count += 1;
                    rest = after;
                    continue 'scan;
                }
            }
        }
        let c = rest.chars().next().unwrap();
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    (out, count)
}

/// Whether the rest of a path, `after` the folder's name, stays inside the
/// folder: `/work/repo/../notes.md` names a file beside it, not in it.
fn stays_inside(after: &str, part_of_name: impl Fn(char) -> bool) -> bool {
    let end = after
        .find(|c: char| !part_of_name(c))
        .unwrap_or(after.len());
    let mut depth = 0usize;
    for part in after[..end].split('/') {
        match part {
            "" | "." => {}
            ".." => match depth.checked_sub(1) {
                Some(up) => depth = up,
                None => return false,
            },
            _ => depth += 1,
        }
    }
    true
}

/// Whether a name ends where `after` starts: at a `/` going on below it,
/// or at anything not part of a path, a sentence's last `.` or `,` too.
fn ends_name(after: &str, part_of_name: impl Fn(char) -> bool) -> bool {
    let mut chars = after.chars();
    match chars.next() {
        None | Some('/') => true,
        Some('.' | ',') => chars.next().is_none_or(char::is_whitespace),
        Some(c) => !part_of_name(c),
    }
}

/// The permissions row of the lead `name`, from its gates and sandbox.
fn lead_mode(agent: &Path, name: &str) -> Option<&'static str> {
    let output = Command::new(agent)
        .args(["inspect", "--bot", name])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let record: Value = serde_json::from_slice(&output.stdout).ok()?;
    crate::approvals::mode(&record)
}

/// Whether the daemon has a task `name`: one whose start failed after it
/// was made keeps its folder, since a retry continues it there.
/// The tasks `starter` started in `project` that are still there, by
/// name and status, as the daemon lists the bots of the project: none when
/// it cannot say.
pub fn still_there(project: &str, starter: &str) -> Vec<(String, String)> {
    let Some(agent) = crate::agent_bin() else {
        return Vec::new();
    };
    let Ok(output) = Command::new(agent)
        .args(["ls", "--name", &format!("{project}.*")])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    match serde_json::from_slice::<Value>(&output.stdout) {
        Ok(Value::Array(bots)) => own(&bots, starter),
        _ => Vec::new(),
    }
}

/// The folders Sonny made the tasks of `project`, from its lead's folder
/// `here`, whose task is no longer there, removed some other way than
/// `remove_task`, such as by `agent rm`: each by name and
/// the space it takes. Found without running anything in them: by their records, or
/// for the worktrees of `here`'s repository, by its list of them. None
/// when the agents can't be listed, so a task still there is never
/// counted gone.
pub fn left_behind(here: &Path, project: &str) -> Vec<(String, Size)> {
    let (Some(agent), Ok(home)) = (crate::agent_bin(), home_dir()) else {
        return Vec::new();
    };
    let Ok(output) = Command::new(agent)
        .args(["ls", "--name", &format!("{project}.*")])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    let Ok(Value::Array(bots)) = serde_json::from_slice::<Value>(&output.stdout) else {
        return Vec::new();
    };
    let folders = home.join(".agent/worktrees");
    let real = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    let here = real(here);
    // The worktrees git lists for the lead's repository, by their real paths.
    let worktrees: Vec<PathBuf> = git(&here, &["worktree", "list", "--porcelain"])
        .map(|list| {
            list.lines()
                .filter_map(|line| line.strip_prefix("worktree "))
                .map(|path| real(Path::new(path)))
                .collect()
        })
        .unwrap_or_default();
    let mut left = Vec::new();
    for entry in std::fs::read_dir(&folders).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(&format!("{project}."))
            || !entry.file_type().is_ok_and(|kind| kind.is_dir())
            || bots.iter().any(|bot| bot["name"] == name.as_str())
        {
            continue;
        }
        let path = entry.path();
        let ours = match recorded(&name) {
            Some((source, _)) => source == here,
            None => worktrees.contains(&real(&path)),
        };
        if ours {
            left.push((name, size(&path)));
        }
    }
    left.sort();
    left
}

/// The space a folder's files take, and whether all were counted: a
/// walk stops after [`SIZE_ENTRIES`], so a large build folder never
/// holds up a start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Size {
    pub bytes: u64,
    pub whole: bool,
}

const SIZE_ENTRIES: usize = 200_000;

/// What `dir` holds, links not followed.
fn size(dir: &Path) -> Size {
    let mut size = Size {
        bytes: 0,
        whole: true,
    };
    let mut folders = vec![dir.to_owned()];
    let mut seen = 0;
    while let Some(folder) = folders.pop() {
        for entry in std::fs::read_dir(&folder).into_iter().flatten().flatten() {
            seen += 1;
            if seen > SIZE_ENTRIES {
                size.whole = false;
                return size;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                folders.push(entry.path());
            } else if meta.is_file() {
                size.bytes += meta.len();
            }
        }
    }
    size
}

impl std::fmt::Display for Size {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let over = if self.whole { "" } else { "over " };
        let bytes = self.bytes as f64;
        match self.bytes {
            0..1_000_000 => write!(f, "{over}{:.0} KB", bytes / 1e3),
            1_000_000..1_000_000_000 => write!(f, "{over}{:.0} MB", bytes / 1e6),
            _ => write!(f, "{over}{:.1} GB", bytes / 1e9),
        }
    }
}

/// Of `bots`, those `lead` made itself, side chats left out.
fn own(bots: &[Value], lead: &str) -> Vec<(String, String)> {
    (bots.iter())
        .filter(|bot| bot["created_by"] == lead && bot["expire_ms"].is_null())
        .filter_map(|bot| {
            let name = bot["name"].as_str()?;
            let status = bot["status"].as_str().unwrap_or("idle");
            Some((name.to_owned(), status.to_owned()))
        })
        .collect()
}

fn listed(agent: &Path, name: &str) -> bool {
    let Ok(output) = Command::new(agent)
        .args(["ls", "--name", name])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return true;
    };
    match serde_json::from_slice::<Value>(&output.stdout) {
        Ok(Value::Array(bots)) => bots.iter().any(|bot| bot["name"] == name),
        // Unreadable: keep the folder rather than lose a task's work.
        _ => true,
    }
}

fn undo(undo: &Undo) {
    match undo {
        Undo::Nothing => {}
        Undo::Worktree { here, path, branch } => crate::snapshot::unwind(here, path, branch),
        Undo::Folder { path, record } => {
            let _ = std::fs::remove_dir_all(path);
            let _ = std::fs::remove_file(record);
        }
    }
}

/// Every task works in the lead's folder.
fn in_place(here: &Path, name: &str) -> Place {
    Place {
        dir: here.to_owned(),
        told: here.to_owned(),
        note: Some(format!(
            "{name}: works in this folder itself, as every task of this project does"
        )),
        undo: Undo::Nothing,
        copy: None,
    }
}

fn home_dir() -> Result<PathBuf, String> {
    Ok(PathBuf::from(
        std::env::var_os("HOME").ok_or("HOME is not set")?,
    ))
}

/// The folder `folder` names inside `here`, by its real path: relative to
/// `here`, or absolute.
fn inside(here: &Path, folder: &str) -> Result<PathBuf, String> {
    let real = std::fs::canonicalize(here.join(folder)).map_err(|e| format!("{folder}: {e}"))?;
    let base = std::fs::canonicalize(here).unwrap_or_else(|_| here.to_owned());
    if !real.is_dir() || !real.starts_with(&base) {
        return Err(format!(
            "{folder} is not a folder inside {}",
            base.display()
        ));
    }
    Ok(real)
}

/// The checkout to run git in for the worktree `path`: `here`'s own, when
/// `path` is a worktree of its repository.
fn repository_of(here: &Path, path: &Path) -> Option<PathBuf> {
    let common = |dir: &Path| {
        git(
            dir,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .ok()
        .and_then(|dir| std::fs::canonicalize(dir).ok())
    };
    if let Some(mine) = common(here)
        && common(path) == Some(mine)
    {
        let top = git(here, &["rev-parse", "--show-toplevel"]).ok()?;
        return std::fs::canonicalize(top).ok();
    }
    None
}

/// Where Sonny records the lead's folder each task with a folder of its
/// own, or a worktree of a repository inside the lead's, was started
/// from, by the task's name: under a name no task's folder can have, as a
/// branch's part never starts with a dot.
fn record_of(name: &str) -> Result<PathBuf, String> {
    Ok(home_dir()?.join(".agent/worktrees/.folders").join(name))
}

/// What Sonny made a task it keeps a record of: recorded, as whatever the
/// task makes in its folder, a repository too, cannot tell.
#[derive(PartialEq)]
enum Made {
    /// An empty folder of its own.
    Own,
    /// A worktree of the repository checked out at this folder inside the
    /// lead's.
    Worktree(PathBuf),
}

/// The lead's folder the task `name` was started from, as Sonny recorded
/// it, and what Sonny made it.
fn recorded(name: &str) -> Option<(PathBuf, Made)> {
    let text = std::fs::read_to_string(record_of(name).ok()?).ok()?;
    let mut lines = text.lines();
    let source = PathBuf::from(lines.next()?);
    let made = match lines.next()? {
        "own" => Made::Own,
        repo => Made::Worktree(PathBuf::from(repo)),
    };
    Some((source, made))
}

fn source_of(name: &str) -> Option<PathBuf> {
    recorded(name).map(|(source, _)| source)
}

/// The lead's folder a task's folder at or above `dir` was made from, when
/// Sonny recorded it, whose project the task works for.
pub fn source_folder(dir: &Path) -> Option<PathBuf> {
    let tasks = home_dir().ok()?.join(".agent/worktrees");
    let tasks = std::fs::canonicalize(&tasks).unwrap_or(tasks);
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_owned());
    let name = dir.strip_prefix(&tasks).ok()?.components().next()?;
    source_of(name.as_os_str().to_str()?)
}

/// An empty folder of the task's own, beside the worktrees: what it works
/// in, such as a workspace the person's tooling makes, it makes there, and
/// it reads `here` where it is.
fn own_folder(here: &Path, name: &str) -> Result<Place, String> {
    let path = home_dir()?.join(".agent/worktrees").join(name);
    let record = record_of(name)?;
    let undo = if path.exists() {
        // Left by a task of this name before: its own only if it was
        // started from this folder.
        if recorded(name) != Some((here.to_owned(), Made::Own)) {
            return Err(format!(
                "{} exists and is not the folder of a task of this one; pick another name",
                path.display()
            ));
        }
        Undo::Nothing
    } else {
        let made = std::fs::create_dir_all(&path)
            .and_then(|()| std::fs::create_dir_all(record.parent().unwrap_or(Path::new("/"))))
            .and_then(|()| std::fs::write(&record, format!("{}\nown\n", here.display())));
        let undo = Undo::Folder {
            path: path.clone(),
            record,
        };
        if let Err(e) = made {
            self::undo(&undo);
            return Err(format!("{}: {e}", path.display()));
        }
        undo
    };
    Ok(Place {
        dir: path.clone(),
        told: here.to_owned(),
        note: Some(format!(
            "{name}: works in {}, an empty folder of its own; it reads this folder, {}, where \
             it is, and makes in its own what it changes, as its brief says how",
            shell_quote(&path),
            shell_quote(here)
        )),
        undo,
        copy: None,
    })
}

/// A git worktree on branch agent/NAME at the last commit, in the lead's
/// subfolder of it. Outside git, the lead's folder, which the task shares.
///
/// `confine`: git runs in a sandbox, as for a lead with one.
fn worktree(here: &Path, name: &str, base: &str, confine: bool) -> Result<Place, String> {
    if git(here, &["rev-parse", "--show-toplevel"]).is_err() {
        return Ok(Place {
            note: Some(format!(
                "{name}: works in this folder itself, shared with you and its other tasks, since \
                 it is not a git repository; a task that needs a copy of its own, such as a \
                 workspace its tooling makes, takes own_folder"
            )),
            ..in_place(here, name)
        });
    }
    let top = PathBuf::from(git(here, &["rev-parse", "--show-toplevel"])?);
    let prefix = git(here, &["rev-parse", "--show-prefix"])?;
    let path = home_dir()?.join(".agent/worktrees").join(name);
    let branch = format!("agent/{name}");
    let undo = if path.exists() {
        // Left by a task of this name before: its own only if it is a
        // worktree of this repository on the task's branch. A folder of a
        // task's own is not, and Sonny runs no git in what the task made.
        if recorded(name).is_some_and(|(_, made)| made == Made::Own) || !ours(here, &path, &branch)
        {
            return Err(format!(
                "{} exists and is not this repository's worktree on {branch}; pick another name",
                path.display()
            ));
        }
        Undo::Nothing
    } else if crate::snapshot::plain(
        here,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .is_ok()
    {
        // Kept from a task of this name removed before, with commits not
        // merged: not this start's to make, nor to delete if it fails.
        return Err(format!(
            "branch {branch} already exists, kept from a task of this name; pick another name, \
             or delete it with `git branch -D {branch}` once its work is merged or not wanted"
        ));
    } else {
        let target = path.to_string_lossy();
        let args = ["worktree", "add", "-q", "-b", &branch, &target, base];
        let undo = Undo::Worktree {
            here: here.to_owned(),
            path: path.clone(),
            branch: branch.clone(),
        };
        match confine {
            true => {
                if let Err(e) = add_confined(sandboxer(), here, &branch, &path, base) {
                    self::undo(&undo);
                    let _ = std::fs::remove_dir_all(&path);
                    return Err(e);
                }
            }
            false => {
                git(here, &args)?;
            }
        }
        undo
    };
    let dir = within(&path, Path::new(prefix.trim_end_matches('/')));
    Ok(Place {
        told: dir.clone(),
        dir,
        note: Some(format!(
            "{name}: a worktree at {} on branch {branch}, from the last commit (uncommitted \
             changes here are not in it); its commits are on {branch} here as it makes \
             them",
            shell_quote(&path)
        )),
        undo,
        copy: Some((top, path)),
    })
}

/// Whether a worktree holds a submodule checked out, which git will not
/// remove unless forced, since the submodule's repository goes with it.
/// Git's own check (`validate_no_submodules`), run in the sandbox, as
/// removal skips git's with `--force`: a `modules` folder in the
/// worktree's git folder, or a submodule in its index with a `.git`.
fn has_submodules(path: &Path) -> Result<bool, String> {
    let git_dir = confined(path, &["rev-parse", "--absolute-git-dir"], &[])?;
    if Path::new(&git_dir).join("modules").is_dir() {
        return Ok(true);
    }
    let staged = confined_bytes(
        path,
        &["--no-optional-locks", "ls-files", "--stage", "-z"],
        &[],
    )?;
    // By the bytes of each name, which need not be UTF-8.
    use std::os::unix::ffi::OsStrExt;
    Ok(staged
        .split(|byte| *byte == 0)
        .filter_map(|entry| entry.strip_prefix(b"160000 "))
        .filter_map(|entry| Some(&entry[entry.iter().position(|byte| *byte == b'\t')? + 1..]))
        .any(|name| {
            path.join(std::ffi::OsStr::from_bytes(name))
                .join(".git")
                .exists()
        }))
}

/// A worktree of `here`'s repository at `path` on the new `branch`, made
/// by git running none of the repository's code (no checkout, hook or
/// fsmonitor), then checked out with `agent` (none: no sandbox here) in
/// the sandbox, writing only the worktree and its own git folder, off the
/// network (an LFS file comes from the objects the repository holds). The
/// checkout is what runs the repository's filters and its post-checkout
/// hook, which a repository an agent made inside the lead's folder holds
/// as the agent wrote them. The shared git folder is never written from
/// the sandbox, so its config need not be one the sandbox can keep
/// read-only, as one that includes other files is not.
fn add_confined(
    agent: Option<&Path>,
    here: &Path,
    branch: &str,
    path: &Path,
    base: &str,
) -> Result<(), String> {
    let target = path.to_string_lossy();
    let add = [
        "worktree",
        "add",
        "-q",
        "--no-checkout",
        "-b",
        branch,
        &target,
        base,
    ];
    crate::snapshot::plain(here, &add)?;
    let own = crate::snapshot::plain(path, &["rev-parse", "--absolute-git-dir"])?;
    let own = std::fs::canonicalize(own.trim()).map_err(|e| format!("{own}: {e}"))?;
    confined_in(agent, path, &["checkout", "-f", "-q"], &[path, &own]).map(drop)
}

/// Whether `dir` is in a worktree Sonny made a task.
pub fn in_worktree(dir: &Path) -> bool {
    let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_owned());
    home_dir().is_ok_and(|home| real(dir).starts_with(real(&home.join(".agent/worktrees"))))
}

/// Whether `top` is the top of one of Sonny's task worktrees itself, whose
/// git config no task can write, and not a repository made inside one.
pub fn is_worktree(top: &Path) -> bool {
    let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_owned());
    home_dir().is_ok_and(|home| {
        real(top).parent() == Some(&real(&home.join(".agent/worktrees")))
            && top.join(".git").is_file()
    })
}

/// The bot `name` as the daemon lists it.
/// The bot `name` as the daemon lists it, read with the CLI.
pub fn record_of_bot(name: &str) -> Option<Value> {
    record(&crate::agent_bin()?, name)
}

fn record(agent: &Path, name: &str) -> Option<Value> {
    let output = Command::new(agent)
        .args(["ls", "--name", name])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let bots: Vec<Value> = serde_json::from_slice(&output.stdout).ok()?;
    bots.into_iter().find(|bot| bot["name"] == name)
}

/// The task of `project` named `from` that a start forks, and its files
/// after `start.turn`, or after its newest turn that kept them.
fn source(agent: &Path, project: &str, from: &str, start: &Start) -> Result<Source, String> {
    if start.profile.is_some() || start.model.is_some() || start.effort.is_some() {
        return Err("a fork keeps its source's role, model and effort; name none".into());
    }
    let name = crate::resolved::call(&serde_json::json!({"task": from}), project)
        .ok_or("from names a task, without /")?;
    let bot = record(agent, &name).ok_or_else(|| format!("{name} is not an agent here"))?;
    let workspace = PathBuf::from(bot["workspace"].as_str().unwrap_or_default());
    if !in_worktree(&workspace) {
        return Err(format!(
            "{name} works in no worktree of its own, so no turn of it kept its files"
        ));
    }
    let bot_id = bot["bot_id"]
        .as_i64()
        .ok_or("the daemon listed no bot_id")?;
    // Its repository, which need not be the lead's: a lead's folder may
    // hold several, or be in none.
    let top = crate::snapshot::top(&workspace)
        .filter(|top| is_worktree(top))
        .ok_or_else(|| format!("{name} works in no worktree of its own"))?;
    let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_owned());
    let now = (real(&workspace).strip_prefix(real(&top)))
        .map_err(|_| format!("{name} works outside its worktree"))?
        .to_owned();
    let refs = crate::snapshot::refs(&crate::daemon_paths(None, None)?.0);
    let snapshot = crate::snapshot::find(&top, &refs, bot_id, start.turn)
        .map_err(|e| format!("{name}: {e}"))?;
    // Where it worked in that turn, which may not be where it works now.
    let prefix = snapshot.folder.clone().unwrap_or(now);
    Ok(Source {
        name,
        top,
        prefix,
        snapshot,
    })
}

/// A git worktree of `source`'s repository on branch agent/NAME holding
/// the files it had after its turn, on the commit it was on, in the folder
/// of it `source` worked in.
fn forked(agent: &Path, name: &str, source: &Source) -> Result<Place, String> {
    // The repository's own checkout, whose paths a brief names; a bare
    // one has none, and the source's worktree stands in.
    let args = ["rev-parse", "--path-format=absolute", "--git-common-dir"];
    let common = PathBuf::from(git(&source.top, &args)?);
    let here = match common.file_name().is_some_and(|name| name == ".git") {
        true => common.parent().map_or(source.top.clone(), Path::to_owned),
        false => source.top.clone(),
    };
    let path = home_dir()?.join(".agent/worktrees").join(name);
    let branch = format!("agent/{name}");
    if path.exists() {
        return Err(format!("{} exists; pick another name", path.display()));
    }
    let indexes = crate::snapshot::indexes(&crate::daemon_paths(None, None)?.0);
    // A bot that can write the sealed folder the files come back through
    // could change it between its making and git's reading of it.
    let output = Command::new(agent)
        .arg("ls")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("running {}: {e}", agent.display()))?;
    let bots: Vec<Value> = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("{} ls: {e}", agent.display()))?;
    let folders: Vec<PathBuf> = (bots.iter())
        .flat_map(crate::snapshot::writable)
        .chain(crate::snapshot::shared())
        .collect();
    if let Some(folder) = crate::snapshot::holder(&indexes, folders.iter().map(PathBuf::as_path)) {
        return Err(format!(
            "{} is inside {}, where a bot works, so no fork's files come back through it",
            indexes.display(),
            folder.display()
        ));
    }
    crate::snapshot::restore(&here, &source.snapshot, &path, &branch, &indexes)?;
    let mut note = format!(
        "{name}: a worktree at {} on branch {branch}, with {}'s files as they were after its \
         turn {}, changes since its last commit uncommitted as they were",
        shell_quote(&path),
        source.name,
        source.snapshot.turn
    );
    if source.snapshot.late {
        note.push_str(
            "; an agent was working in that folder again as they were kept, so they may hold \
             some of that later work",
        );
    }
    let dir = within(&path, &source.prefix);
    Ok(Place {
        told: dir.clone(),
        dir,
        note: Some(note),
        undo: Undo::Worktree {
            here: here.clone(),
            path: path.clone(),
            branch,
        },
        copy: Some((here, path)),
    })
}

/// Whether `path` is a worktree of the repository `here` is in, on `branch`.
fn ours(here: &Path, path: &Path, branch: &str) -> bool {
    let common = |dir: &Path| {
        git(
            dir,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .ok()
        .and_then(|dir| std::fs::canonicalize(dir).ok())
    };
    common(path).is_some_and(|theirs| common(here) == Some(theirs))
        && git(path, &["branch", "--show-current"]).is_ok_and(|current| current == branch)
}

pub fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("running git: {e}"))?;
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git {}: {}", args.join(" "), said.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// git in `dir`, a repository an agent may have written, run as an
/// agent's command is: git runs a repository's hooks and the programs its
/// config names (an fsmonitor, a filter), even to list its files, so in a
/// sandbox writing only `writable` (none: read-only), off the network and
/// so every Unix socket, the daemon's too, as Codex's agents run git
/// (#597). As Sonny itself on a machine with no sandbox, where no agent is
/// confined either.
pub fn confined(dir: &Path, args: &[&str], writable: &[&Path]) -> Result<String, String> {
    let out = confined_bytes(dir, args, writable)?;
    Ok(String::from_utf8_lossy(&out).trim().to_owned())
}

/// [`confined`]'s output as git wrote it, for output split on NULs.
pub fn confined_bytes(dir: &Path, args: &[&str], writable: &[&Path]) -> Result<Vec<u8>, String> {
    confined_in(sandboxer(), dir, args, writable)
}

/// [`confined_bytes`] with Agent's binary `agent` running the sandbox, or
/// none.
fn confined_in(
    agent: Option<&Path>,
    dir: &Path,
    args: &[&str],
    writable: &[&Path],
) -> Result<Vec<u8>, String> {
    let git = crate::snapshot::git_program();
    let mut command = match agent {
        Some(agent) => {
            let mut command = Command::new(agent);
            command.arg("sandbox");
            for path in writable {
                command.arg("--writable").arg(path);
            }
            command.arg("--").arg(git);
            command
        }
        None => Command::new(git),
    };
    command
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null());
    // None of the keys Agent keeps, as a bot's shell has none.
    for key in kept_keys() {
        command.env_remove(key);
    }
    let output = command.output().map_err(|e| format!("running git: {e}"))?;
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git {}: {}", args.join(" "), said.trim()));
    }
    Ok(output.stdout)
}

/// Agent's binary, when it runs a sandbox on this machine, found once: one
/// that ran git in it, which says its version.
fn sandboxer() -> Option<&'static Path> {
    static FOUND: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    FOUND
        .get_or_init(|| {
            let agent = crate::agent_bin()?;
            Command::new(&agent)
                .args(["sandbox", "--"])
                .arg(crate::snapshot::git_program())
                .arg("--version")
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .is_ok_and(|out| out.status.success() && out.stdout.starts_with(b"git version"))
                .then_some(agent)
        })
        .as_deref()
}

/// The folder in `copy` at `prefix`, without the trailing slash an empty
/// one would add.
fn within(copy: &Path, prefix: &Path) -> PathBuf {
    match prefix.as_os_str().is_empty() {
        true => copy.to_owned(),
        false => copy.join(prefix),
    }
}

/// The provider credentials this process may hold, as the daemon keeps
/// them from a bot's shell: the well-known keys, Bedrock's, the key a
/// provider in `AGENT_PROVIDER` names, and what `~/.agent/env` sets.
pub(crate) fn kept_keys() -> Vec<String> {
    let mut keys: Vec<String> = [
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "OPENROUTER_API_KEY",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
        "AWS_BEARER_TOKEN_BEDROCK",
    ]
    .map(String::from)
    .into();
    keys.extend(provider_keys(
        &std::env::var("AGENT_PROVIDER").unwrap_or_default(),
    ));
    if let Some(file) = agent_client::env_file::path()
        && let Ok(set) = agent_client::env_file::read(&file)
    {
        keys.extend(set.iter().map(|(key, _)| key.clone()));
        let named = set.iter().find(|(key, _)| key == "AGENT_PROVIDER");
        keys.extend(
            named
                .map(|(_, specs)| provider_keys(specs))
                .unwrap_or_default(),
        );
    }
    keys
}

/// The key variables `NAME=FAMILY,URL,KEY_ENV` specs name.
fn provider_keys(specs: &str) -> Vec<String> {
    (specs.split_whitespace())
        .filter_map(|spec| spec.split(',').nth(2))
        .filter(|key| !key.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The last 20 lines with words `stream` writes, each cut to 1000 bytes,
/// read to its end without keeping the rest.
fn tail(stream: impl std::io::Read) -> Vec<String> {
    use std::io::BufRead;
    let mut reader = std::io::BufReader::new(stream);
    let mut lines = std::collections::VecDeque::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        // A longer line is read as several.
        match std::io::Read::take(&mut reader, 1000).read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let text = String::from_utf8_lossy(&line).trim_end().to_owned();
        if text.is_empty() {
            continue;
        }
        if lines.len() == 20 {
            lines.pop_front();
        }
        lines.push_back(text);
    }
    lines.into()
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', r"'\''"))
}

/// A `remove_task` call: remove the task and the worktree Sonny made it,
/// once its changes are committed, its branch kept unless merged; a folder
/// of its own only with `discard`, once the task says its work is pushed.
/// `discard` removes them anyway. An agent still working is not removed,
/// and then neither is anything else. Says what was removed and what was
/// kept.
pub fn remove(here: &Path, name: &str, discard: bool) -> Result<String, String> {
    let agent = crate::agent_bin().ok_or("Agent's binary was not found; set AGENT_BIN")?;
    let branch = format!("agent/{name}");
    let path = home_dir()?.join(".agent/worktrees").join(name);
    let (worktree, folder) = match (path.exists(), recorded(name)) {
        (false, _) => (None, None),
        (true, Some((source, _))) if source != here => {
            return Err(format!(
                "{} is the folder of a task of {}; run this there",
                path.display(),
                source.display()
            ));
        }
        (true, Some((_, Made::Own))) => (None, Some(path)),
        (true, Some((_, Made::Worktree(repo)))) if ours(&repo, &path, &branch) => {
            (Some((repo, path)), None)
        }
        (true, None)
            if let Some(repo) = repository_of(here, &path)
                && ours(&repo, &path, &branch) =>
        {
            (Some((repo, path)), None)
        }
        (true, _) => {
            return Err(format!(
                "{} is not this repository's worktree on {branch}; run this in the repository \
                 the task was started from",
                path.display()
            ));
        }
    };
    // What would be lost, checked while the task can still be kept, and again
    // once it is gone and can write no more.
    let kept = || -> Result<(), String> {
        if !discard
            && let Some((_, path)) = &worktree
            && !confined(
                path,
                &[
                    "--no-optional-locks",
                    "status",
                    "--porcelain",
                    "--untracked-files=all",
                    "--ignore-submodules=none",
                ],
                &[],
            )?
            .is_empty()
        {
            return Err(format!(
                "{name}'s worktree has uncommitted changes; commit them, or with discard, it is \
                 removed anyway"
            ));
        }
        if !discard
            && let Some((_, path)) = &worktree
            && has_submodules(path)?
        {
            return Err(format!(
                "{name}'s worktree has submodules checked out, whose repositories go with it; \
                 push their work, then with discard it is removed anyway"
            ));
        }
        // What the task made there, Sonny cannot check without running it:
        // a repository's own settings name programs git runs as the person,
        // outside the task's sandbox. So the task says, not Sonny.
        if !discard && folder.is_some() {
            return Err(format!(
                "{name}'s folder holds what it made, which Sonny cannot check for work not \
                 committed or pushed without running it; ask {name} whether its work is pushed, \
                 then remove it with discard"
            ));
        }
        Ok(())
    };
    kept()?;
    let bot_id = record(&agent, name).and_then(|bot| bot["bot_id"].as_i64());
    if listed(&agent, name) {
        let output = crate::agent_command(&agent)?
            .args(["rm", "--bot", name])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("running {}: {e}", agent.display()))?;
        if !output.status.success() {
            let said = String::from_utf8_lossy(&output.stderr);
            return Err(format!("{name} was not removed: {}", said.trim()));
        }
        kept().map_err(|e| format!("{name}'s bot was removed, but {e}"))?;
    }
    if let Some((repo, path)) = &worktree {
        // Checked for changes and submodules above, in the sandbox: git's own check would
        // run the repository's fsmonitor and filters as the person, so it
        // is skipped (`--force`, which still keeps a locked worktree), and
        // no hook runs.
        let path = path.to_string_lossy();
        crate::snapshot::plain(repo, &["worktree", "remove", "--force", &path])?;
        if let Ok(record) = record_of(name) {
            let _ = std::fs::remove_file(record);
        }
        // Its snapshots go with it once its folder is gone, so one still
        // being taken of it fails rather than lands after.
        if let Some(bot_id) = bot_id {
            let refs = crate::snapshot::refs(&crate::daemon_paths(None, None)?.0);
            crate::snapshot::forget(repo, &refs, bot_id)?;
        }
        let delete = if discard { "-D" } else { "-d" };
        let ours = git(here, &["rev-parse", "--show-toplevel"])
            .ok()
            .and_then(|top| std::fs::canonicalize(top).ok())
            .is_some_and(|top| &top == repo);
        if crate::snapshot::plain(repo, &["branch", delete, &branch]).is_err() {
            return Ok(match ours {
                true => format!(
                    "Removed {name}. {branch} is kept, since it has commits not merged here; \
                     `git branch -D {branch}` deletes it."
                ),
                false => format!(
                    "Removed {name}. {branch} is kept in {}, since it has commits not merged \
                     there; `git -C {} branch -D {branch}` deletes it.",
                    repo.display(),
                    shell_quote(repo)
                ),
            });
        }
    }
    if let Some(path) = &folder {
        std::fs::remove_dir_all(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if let Ok(record) = record_of(name) {
            let _ = std::fs::remove_file(record);
        }
    }
    Ok(format!("Removed {name}."))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Agent's binary built beside these tests, when it runs a sandbox
    /// here, and a folder beside it outside temporary storage, which every
    /// writable sandbox may write.
    fn sandbox_here(name: &str) -> Option<(PathBuf, PathBuf)> {
        let built = std::env::current_exe().ok()?.parent()?.parent()?.to_owned();
        let agent = built.join("agent");
        let runs = Command::new(&agent)
            .args(["sandbox", "--", "true"])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        let temporary = std::fs::canonicalize(std::env::temp_dir()).ok()?;
        let built = std::fs::canonicalize(&built).ok()?;
        if !runs || built.starts_with(&temporary) || built.starts_with("/tmp") {
            eprintln!("skipped: no sandbox here, or the build is in temporary storage");
            return None;
        }
        let dir = built.join(format!("sonny-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).ok()?;
        Some((agent, std::fs::canonicalize(&dir).ok()?))
    }

    fn repository(dir: &Path) -> PathBuf {
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["add", "a.txt"],
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                "a",
            ],
        ] {
            git(&repo, args).unwrap();
        }
        repo
    }

    #[test]
    fn a_submodule_is_found_whatever_bytes_its_name_has() {
        use std::os::unix::ffi::OsStrExt;
        let dir = std::env::temp_dir().join(format!("sonny-submodule-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let repo = repository(&dir);
        assert!(!has_submodules(&repo).unwrap());
        let head = git(&repo, &["rev-parse", "HEAD"]).unwrap();
        let name = std::ffi::OsStr::from_bytes(b"sub-\xff");
        let entry = format!("160000,{head},");
        let mut entry = std::ffi::OsString::from(entry);
        entry.push(name);
        let added = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["update-index", "--add", "--cacheinfo"])
            .arg(&entry)
            .status()
            .unwrap();
        assert!(added.success());
        assert!(!has_submodules(&repo).unwrap(), "not checked out");
        std::fs::create_dir_all(repo.join(name).join(".git")).unwrap();
        assert!(has_submodules(&repo).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn what_an_agent_left_in_a_repository_runs_only_in_the_sandbox() {
        let Some((agent, dir)) = sandbox_here("confined") else {
            return;
        };
        let repo = repository(&dir);
        let marker = dir.join("ran");
        // An fsmonitor runs to list the files, even for `status`.
        let command = format!("touch {}; false", marker.display());
        git(&repo, &["config", "core.fsmonitor", &command]).unwrap();
        let status = ["--no-optional-locks", "status", "--porcelain"];
        git(&repo, &status).unwrap();
        assert!(marker.exists(), "git runs it as Sonny");
        std::fs::remove_file(&marker).unwrap();
        confined_in(Some(&agent), &repo, &status, &[]).unwrap();
        assert!(!marker.exists(), "the sandbox keeps it from writing");
        git(&repo, &["config", "--unset", "core.fsmonitor"]).unwrap();

        // A hook runs on a worktree's checkout, writing only the worktree
        // and its own git folder, even where the repository's config
        // includes another file, which the sandbox can't keep read-only.
        git(&repo, &["config", "include.path", "shared.config"]).unwrap();
        let hook = repo.join(".git/hooks/post-checkout");
        std::fs::write(&hook, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
        std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let tree = dir.join("tree");
        // The hook's write fails, so git says the checkout failed.
        let made = add_confined(Some(&agent), &repo, "agent/t", &tree, "HEAD");
        assert!(made.is_err_and(|e| e.contains("touch")), "the hook ran");
        assert!(tree.join("a.txt").is_file(), "the worktree is checked out");
        assert!(!marker.exists(), "the hook ran in the sandbox");
        std::fs::remove_file(&hook).unwrap();
        let other = dir.join("other");
        add_confined(Some(&agent), &repo, "agent/u", &other, "HEAD").unwrap();
        assert!(other.join("a.txt").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_lead_s_tasks_still_there_are_those_it_made_itself() {
        use serde_json::json;
        let bots = [
            json!({"name": "shop.lead", "status": "running"}),
            json!({"name": "shop.fix", "status": "idle", "created_by": "shop.lead"}),
            json!({"name": "shop.docs", "status": "running", "created_by": "shop.lead"}),
            json!({"name": "shop.fix.check", "status": "idle", "created_by": "shop.fix"}),
            json!({"name": "shop.invoices", "status": "idle", "created_by": "shop.lead.billing"}),
            json!({"name": "shop.lead.side-1a2b3c", "status": "idle",
                "created_by": "shop.lead", "expire_ms": 900_000}),
        ];
        assert_eq!(
            own(&bots, "shop.lead"),
            [
                ("shop.fix".to_owned(), "idle".to_owned()),
                ("shop.docs".to_owned(), "running".to_owned())
            ]
        );
    }

    #[test]
    fn a_provider_spec_s_key_is_kept_from_setup() {
        assert_eq!(
            provider_keys("openai=responses,http://x/v1,WORK_KEY local=chat,http://y"),
            ["WORK_KEY"]
        );
    }

    #[test]
    fn a_setup_s_errors_keep_only_their_last_lines() {
        let mut said: String = (1..=30).map(|n| format!("line {n}\n")).collect();
        said.push_str(&"x".repeat(2500));
        said.push_str("\n\n");
        let kept = tail(said.as_bytes());
        assert_eq!(kept.len(), 20);
        assert_eq!(kept[0], "line 14");
        assert_eq!(kept[16], "line 30");
        // A long line is kept in pieces of 1000 bytes.
        assert_eq!(
            kept[17..].iter().map(String::len).collect::<Vec<_>>(),
            [1000, 1000, 500]
        );
    }

    #[test]
    fn a_brief_names_the_copy_where_it_named_the_folder() {
        let names = ["/Volumes/workplace".to_owned(), "~/workplace".to_owned()];
        let to = "/Users/g/.agent/tasks/t";
        let said = "Write /Volumes/workplace/notes.md and ~/workplace/a, not /Volumes/workplace2, \
                    /x/Volumes/workplace or ~/workplaces. Look in /Volumes/workplace.";
        let (brief, count) = replace_paths(said, &names, to);
        assert_eq!(
            brief,
            "Write /Users/g/.agent/tasks/t/notes.md and /Users/g/.agent/tasks/t/a, not /Volumes/workplace2, \
             /x/Volumes/workplace or ~/workplaces. Look in /Users/g/.agent/tasks/t."
        );
        assert_eq!(count, 3);
        assert_eq!(replace_paths("`/Volumes/workplace`", &names, to).1, 1);
        // Paths that climb out of the folder name what is beside it.
        for beside in [
            "/Volumes/workplace/../notes.md",
            "/Volumes/workplace/a/../../b",
        ] {
            assert_eq!(replace_paths(beside, &names, to), (beside.to_owned(), 0));
        }
        assert_eq!(
            replace_paths("/Volumes/workplace/a/../b", &names, to).0,
            "/Users/g/.agent/tasks/t/a/../b"
        );
        // Sibling folders whose names only start the same stay as they are.
        for sibling in [
            "/Volumes/workplace+backup",
            "/Volumes/workplace@old",
            "/Volumes/workplace#2",
        ] {
            assert_eq!(replace_paths(sibling, &names, to), (sibling.to_owned(), 0));
        }
        assert_eq!(
            replace_paths(
                "--dir=/Volumes/workplace:/Volumes/workplace/b (see ~/workplace)",
                &names,
                to
            )
            .0,
            "--dir=/Users/g/.agent/tasks/t:/Users/g/.agent/tasks/t/b (see /Users/g/.agent/tasks/t)"
        );
    }

    #[test]
    fn a_task_runs_as_the_person_set_tasks_up() {
        let run = |info: Value, flags: &[&str], task: Option<&str>| {
            let flags: Vec<String> = flags.iter().map(|f| f.to_string()).collect();
            let lead = (Some("a/lead".to_owned()), Some("high".to_owned()));
            with_defaults(
                &info,
                &flags.iter().collect::<Vec<_>>(),
                lead,
                task.map(str::to_owned),
            )
        };
        let none = serde_json::json!({});
        // Nothing set: the lead's own model and effort, as before.
        assert_eq!(
            run(none.clone(), &["--agents"], None),
            ["--agents", "--model", "a/lead", "--effort", "high"]
        );
        // The config's task effort over the lead's effort.
        assert_eq!(
            run(none.clone(), &[], Some("xhigh")),
            ["--model", "a/lead", "--effort", "xhigh"]
        );
        // The project's file over both.
        let file = serde_json::json!({"threads_effort": "medium"});
        assert_eq!(
            run(file, &[], Some("xhigh")),
            ["--model", "a/lead", "--effort", "medium"]
        );
        // Another model leaves the lead's effort, which was set for its own.
        let file = serde_json::json!({"threads_model": "b/other"});
        assert_eq!(run(file.clone(), &[], None), ["--model", "b/other"]);
        assert_eq!(
            run(file, &[], Some("low")),
            ["--model", "b/other", "--effort", "low"]
        );
        // What the lead names is kept.
        assert_eq!(
            run(
                none.clone(),
                &["--model=c/d", "--effort", "max"],
                Some("low")
            ),
            ["--model=c/d", "--effort", "max"]
        );
        assert_eq!(
            run(none, &["--model", "a/lead"], None),
            ["--model", "a/lead", "--effort", "high"]
        );
    }

    #[test]
    fn a_claude_task_keeps_its_cache_for_an_hour() {
        let run = |flags: &[&str]| {
            let mut flags: Vec<String> = flags.iter().map(|f| f.to_string()).collect();
            with_cache(&mut flags);
            flags
        };
        assert_eq!(
            run(&["--model", "anthropic/claude-opus-5-5"]),
            ["--model", "anthropic/claude-opus-5-5", "--cache-ttl", "1h"]
        );
        assert_eq!(
            run(&["--model=bedrock/us.anthropic.claude-opus-5-5"]),
            [
                "--model=bedrock/us.anthropic.claude-opus-5-5",
                "--cache-ttl",
                "1h"
            ]
        );
        // Other providers have no such lifetime; one the lead names stays.
        assert_eq!(
            run(&["--model", "openai/gpt-6"]),
            ["--model", "openai/gpt-6"]
        );
        assert_eq!(
            run(&["--model", "anthropic/claude-opus-5-5", "--cache-ttl=5m"]),
            ["--model", "anthropic/claude-opus-5-5", "--cache-ttl=5m"]
        );
        assert_eq!(run(&[]), Vec::<String>::new());
    }

    #[test]
    fn a_task_is_told_who_it_works_for() {
        let dir = std::env::temp_dir().join(format!("sonny-task-told-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".agents/agents")).unwrap();
        std::fs::write(
            dir.join(".agents/agents/reviewer.md"),
            "---\nmodel: a/b\ntools: [shell, read]\n---\nReview it.\n",
        )
        .unwrap();
        let project = std::cell::RefCell::new(serde_json::json!({"threads_profile": "task"}));
        let told = |flags: &[&str]| {
            let flags: Vec<String> = flags.iter().map(|f| f.to_string()).collect();
            told(
                &dir,
                &dir,
                &dir,
                &project.borrow(),
                &flags.iter().collect::<Vec<_>>(),
            )
        };
        let person = |flags: &[String]| {
            let at = flags.iter().position(|f| f == "--instructions").unwrap();
            assert!(flags[at + 1].ends_with("their latest words override both."));
            flags[at + 1].clone()
        };

        let flags = told(&["--agents", "--model", "x/y"]).unwrap();
        assert_eq!(flags[..2], ["--model", "x/y"]);
        assert!(!flags.contains(&"--agents".to_owned()));
        person(&flags);

        let flags = told(&["--profile", "reviewer"]).unwrap();
        assert_eq!(flags[..4], ["--model", "a/b", "--tools", "shell,read"]);
        assert!(person(&flags).contains("Review it."));
        // The task's own picks win over the role's.
        let flags = told(&["--profile", "reviewer", "--model", "x/y"]).unwrap();
        assert_eq!(flags[..4], ["--model", "x/y", "--tools", "shell,read"]);
        let flags = told(&["--profile=reviewer", "--model=x/y", "--tools=read"]).unwrap();
        assert_eq!(
            flags[..3],
            ["--model=x/y", "--tools=read", "--instructions"]
        );

        // The person's task.md replaces Sonny's, after any role, and once.
        std::fs::write(dir.join(".agents/agents/task.md"), "Be kind.\n").unwrap();
        let flags = told(&["--profile", "reviewer"]).unwrap();
        assert!(flags.last().unwrap().ends_with("Review it.\n\nBe kind."));
        let flags = told(&["--profile", "task"]).unwrap();
        assert_eq!(flags.last().unwrap().matches("Be kind.").count(), 1);
        std::fs::remove_file(dir.join(".agents/agents/task.md")).unwrap();

        // A project's tasks told another profile, which builds on Sonny's
        // task text, and is told once even when named as the role too.
        std::fs::write(
            dir.join(".agents/agents/worker.md"),
            "---\nextends: task\n---\nShip small.\n",
        )
        .unwrap();
        *project.borrow_mut() = serde_json::json!({"threads_profile": "worker"});
        let flags = told(&["--agents"]).unwrap();
        let text = flags.last().unwrap();
        assert!(
            text.ends_with("their latest words override both.\n\nShip small."),
            "{text}"
        );
        std::fs::write(
            dir.join(".agents/agents/worker.md"),
            "---\nextends: task\ntools: [read]\n---\nShip small.\n",
        )
        .unwrap();
        let flags = told(&["--profile", "worker"]).unwrap();
        assert_eq!(flags[..2], ["--tools", "read"]);
        assert_eq!(flags.last().unwrap().matches("Ship small.").count(), 1);
        // `task` is then a role like any other, as is another file whose
        // name differs only in case, where the filesystem tells them apart.
        let flags = told(&["--profile", "task"]).unwrap();
        assert!(flags.last().unwrap().contains("# Role: task\n\n"));
        std::fs::write(dir.join(".agents/agents/Worker.md"), "Ship big.\n").unwrap();
        if std::fs::read_to_string(dir.join(".agents/agents/worker.md")).unwrap() != "Ship big.\n" {
            let flags = told(&["--profile", "Worker"]).unwrap();
            assert!(
                flags
                    .last()
                    .unwrap()
                    .contains("# Role: Worker\n\nShip big.")
            );
        }
        std::fs::remove_file(dir.join(".agents/agents/Worker.md")).unwrap();
        *project.borrow_mut() = serde_json::json!({"threads_profile": "nobody"});
        assert!(
            told(&["--agents"])
                .unwrap_err()
                .starts_with("profile_not_found")
        );
        *project.borrow_mut() = serde_json::json!({"threads_profile": "task"});

        assert_eq!(told(&["--model", "x/y"]).unwrap(), ["--model", "x/y"]);
        assert!(told(&["--profile", "nobody"]).is_err());
        assert!(told(&["--agents", "--instructions", "x"]).is_err());
        assert!(told(&["--agents", "--instructions-file=x"]).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
