//! A lead's tasks, each in a folder of its own: the `start_task` tool Sonny
//! answers makes the task a git worktree, then starts it there with the
//! daemon's `agent run`. A worktree starts at the last commit on a branch
//! of its own, with clones of the lead's build folders where the disk makes
//! them copy-on-write (seed.rs); outside git, the task works in the lead's
//! folder. A task that could not start leaves no folder behind, and the
//! `remove_task` tool takes away one that is done.
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The task's folder, how its work comes back, and what undoes it.
struct Place {
    dir: PathBuf,
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
}

/// A `start_task` call.
pub struct Start {
    pub name: String,
    pub brief: String,
    pub profile: Option<String>,
    pub setup: bool,
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// Start the task `start` asks for of the lead `lead`, whose folder is
/// `here`, and say what started and how its work comes back.
pub fn start(here: &Path, lead: &str, start: &Start) -> Result<String, String> {
    let project = lead.strip_suffix(".lead").unwrap_or(lead);
    let short = start.name.trim();
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
    let mut place = if info["threads_in"] == "project" {
        in_place(here)
    } else {
        worktree(here, name)?
    };
    // Only a run that started can have made the task, which then keeps
    // its folder; anything that failed before leaves nothing behind.
    let failed = |place: &Place, ran: bool, error: String| {
        if !ran || !listed(&agent, name) {
            undo(&place.undo);
        }
        Err(error)
    };
    // The folder's setup script, run in the task's own folder, as for a
    // worktree; a task in the lead's folder has nothing to set up.
    let script = here.join(".agents/setup");
    if start.setup && place.dir != here && script.exists() {
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
            .env("AGENT_SOURCE", here)
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
    // the lead's folders hold.
    if let (Undo::Worktree { path, .. }, Some((top, _))) = (&place.undo, &place.copy) {
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
    let lead = (
        std::env::var("AGENT_MODEL").ok(),
        std::env::var("AGENT_EFFORT").ok(),
    );
    let flags: Vec<&String> = flags.iter().collect();
    let flags = with_defaults(&info, &flags, lead, config.sonny.task_effort);
    let flags: Vec<&String> = flags.iter().collect();
    let mut flags = match told(&place.dir, &info, &flags) {
        Ok(flags) => flags,
        Err(error) => return failed(&place, false, error),
    };
    with_cache(&mut flags);
    // Every task works in a sandbox, whatever its lead's mode, and may
    // commit to its repository.
    flags.extend(["--sandbox", "workspace-write", "--network"].map(String::from));
    if let Some(git) = crate::approvals::repository(&place.dir) {
        flags.extend(["--writable".to_owned(), git]);
    }
    // A path in the lead's folder would have the task write there instead
    // of in its copy, where nothing keeps it apart from the lead's work.
    let brief = vec![start.brief.clone()];
    let (brief, moved) = match &place.copy {
        Some((from, to)) => in_copy(&brief, from, to),
        None => (brief, 0),
    };
    let output = command
        .args(["run", "--detach", "--new", "--bot", name, "--workspace"])
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
/// their own. Other flags pass as they are.
fn told(dir: &Path, info: &Value, flags: &[&String]) -> Result<Vec<String>, String> {
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
        Some(name) => Some(crate::project::profile(dir, name)?),
        None => None,
    };
    // The same profile when it is the same file (a case-insensitive
    // filesystem gives `Worker` and `worker` one), or the same built-in.
    let told_after = |role: &&agent_client::policy::Profile| match &role.path {
        None => role.name == task,
        Some(path) => crate::project::profile(dir, &task).is_ok_and(|task| {
            let real = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
            task.path.is_some_and(|other| real(&other) == real(path))
        }),
    };
    let memory = agent_client::policy::memory_indexes(dir).map_err(|e| e.to_string())?;
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
        crate::project::as_task(dir, info, &composed.text)?,
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

/// Whether the daemon has a task `name`: one whose start failed after it
/// was made keeps its folder, since a retry continues it there.
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
        Undo::Worktree { here, path, branch } => {
            let path = path.to_string_lossy();
            let _ = git(here, &["worktree", "remove", "--force", &path]);
            let _ = git(here, &["branch", "-D", branch]);
        }
    }
}

/// Every task works in the lead's folder.
fn in_place(here: &Path) -> Place {
    Place {
        dir: here.to_owned(),
        note: None,
        undo: Undo::Nothing,
        copy: None,
    }
}

fn home_dir() -> Result<PathBuf, String> {
    Ok(PathBuf::from(
        std::env::var_os("HOME").ok_or("HOME is not set")?,
    ))
}

/// A git worktree on branch agent/NAME at the last commit, in the lead's
/// subfolder of it. Outside git the task works in the lead's folder.
fn worktree(here: &Path, name: &str) -> Result<Place, String> {
    if git(here, &["rev-parse", "--show-toplevel"]).is_err() {
        return Ok(in_place(here));
    }
    let top = PathBuf::from(git(here, &["rev-parse", "--show-toplevel"])?);
    let prefix = git(here, &["rev-parse", "--show-prefix"])?;
    let path = home_dir()?.join(".agent/worktrees").join(name);
    let branch = format!("agent/{name}");
    let undo = if path.exists() {
        // Left by a task of this name before: its own only if it is a
        // worktree of this repository on the task's branch.
        if !ours(here, &path, &branch) {
            return Err(format!(
                "{} exists and is not this repository's worktree on {branch}; pick another name",
                path.display()
            ));
        }
        Undo::Nothing
    } else {
        let target = path.to_string_lossy();
        git(
            here,
            &["worktree", "add", "-q", "-b", &branch, &target, "HEAD"],
        )?;
        Undo::Worktree {
            here: here.to_owned(),
            path: path.clone(),
            branch: branch.clone(),
        }
    };
    Ok(Place {
        dir: within(&path, Path::new(prefix.trim_end_matches('/'))),
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
/// once its changes are committed, its branch kept unless merged.
/// `discard` removes them anyway. An agent still working is not removed,
/// and then neither is anything else. Says what was removed and what was
/// kept.
pub fn remove(here: &Path, name: &str, discard: bool) -> Result<String, String> {
    let agent = crate::agent_bin().ok_or("Agent's binary was not found; set AGENT_BIN")?;
    let branch = format!("agent/{name}");
    let worktree = home_dir()?.join(".agent/worktrees").join(name);
    let worktree = match worktree.exists() {
        true if ours(here, &worktree, &branch) => Some(worktree),
        true => {
            return Err(format!(
                "{} is not this repository's worktree on {branch}; run this in the repository \
                 the task was started from",
                worktree.display()
            ));
        }
        false => None,
    };
    // What would be lost, checked while the task can still be kept, and again
    // once it is gone and can write no more.
    let kept = || -> Result<(), String> {
        if !discard
            && let Some(path) = &worktree
            && !git(path, &["status", "--porcelain", "--untracked-files=all"])?.is_empty()
        {
            return Err(format!(
                "{name}'s worktree has uncommitted changes; commit them, or with discard, it is \
                 removed anyway"
            ));
        }
        Ok(())
    };
    kept()?;
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
    if let Some(path) = &worktree {
        let path = path.to_string_lossy();
        let mut args = vec!["worktree", "remove"];
        if discard {
            args.push("--force");
        }
        args.push(&path);
        git(here, &args)?;
        let delete = if discard { "-D" } else { "-d" };
        if git(here, &["branch", delete, &branch]).is_err() {
            return Ok(format!(
                "Removed {name}. {branch} is kept, since it has commits not merged here; \
                 `git branch -D {branch}` deletes it."
            ));
        }
    }
    Ok(format!("Removed {name}."))
}

#[cfg(test)]
mod tests {
    use super::*;

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
            told(&dir, &project.borrow(), &flags.iter().collect::<Vec<_>>())
        };
        let person = |flags: &[String]| {
            let at = flags.iter().position(|f| f == "--instructions").unwrap();
            assert!(flags[at + 1].ends_with("helping them well is the care."));
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
            text.ends_with("helping them well is the care.\n\nShip small."),
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
