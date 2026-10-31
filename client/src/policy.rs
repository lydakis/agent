//! What a human-facing client tells a new bot. The daemon stores whatever
//! text it is given and never composes any; this is where clients agree on
//! the composition, so a bot created from the app or the CLI with `--agents`
//! reads the same way.
//!
//! Layers, in this order: the harness preamble (how to delegate and collect
//! results through this runtime), every AGENTS.md and `.agents/AGENTS.md`
//! from the workspace up to the filesystem root plus the user's global one,
//! an index of skills the bot can open with its `read` tool and of profiles
//! it can start peers in, the memory indexes the client names, and the
//! bot's own role when it is started in one.
//! The text is a stable prefix on purpose: it rides the provider's prompt
//! cache after the first turn, so it changes only when a file changes.
use std::path::{Path, PathBuf};

/// The daemon refuses instructions above 64 KiB; stay under it with room
/// for the prompt cache to matter.
pub const MAX_INSTRUCTIONS: usize = 60 * 1024;

/// The command a bot's shell reaches the daemon's CLI by, as the client
/// that made the bot says: `AGENT_CLI` when a client that wraps the CLI
/// sets it (Sonny sets its own `"PATH/sonny" agent`), else `"$AGENT_BIN"`,
/// the binary the daemon gives every shell.
pub fn cli() -> String {
    std::env::var("AGENT_CLI")
        .ok()
        .filter(|cli| !cli.trim().is_empty())
        .unwrap_or_else(|| "\"$AGENT_BIN\"".into())
}

/// `text` with each `"$AGENT_BIN"` naming the CLI the way `cli` says, as
/// the preamble and a role are written.
pub fn with_cli(text: &str) -> String {
    naming(text, &cli())
}

fn naming(text: &str, cli: &str) -> String {
    text.replace("\"$AGENT_BIN\"", cli)
}

/// `PREAMBLE`, naming the CLI the way `cli` says.
pub fn preamble() -> String {
    with_cli(PREAMBLE)
}

pub const PREAMBLE: &str = "To delegate a subtask to another agent with its own conversation, run \
\"$AGENT_BIN\" run --detach --new --bot NAME -- TASK from the shell; it prints a turn handle immediately. \
Add --agents to give it this folder's AGENTS.md, skills and memory. \
Add --model PROVIDER/MODEL to give it one of the models \"$AGENT_BIN\" models lists; without it, it gets yours. \
Continue an existing agent with \"$AGENT_BIN\" run --detach --bot NAME -- TASK. \
To tell an agent something that needs no reply, such as a change of scope it should take into account, \
add --delivery attach: the message joins its next model call, mid-turn or in its next turn, without starting a turn of its own. \
Collect results with the wait tool on that handle; it returns the peer's status and final text. \
Blocking run/follow inside a shell tool is rejected. \
Use \"$AGENT_BIN\" fork --source NAME --bot NEW to branch an agent from where it is now, mid-turn included; --checkpoint N branches from an earlier point in its history. \
To ask an agent something without disturbing it, mid-turn included, run \
\"$AGENT_BIN\" run --detach --fork NAME --bot NEW --allow '' --expires 15m -- QUESTION and wait on its handle: \
a copy of NAME answers from its history without acting, and is deleted once it has been idle 15 minutes. \
$AGENT_BOT and $AGENT_BOT_ID identify you; $AGENT_PARENT and $AGENT_PARENT_ID, when set, identify the agent that created you. \
To contact that creator, run \
\"$AGENT_BIN\" run --detach --bot \"$AGENT_PARENT\" --bot-id \"$AGENT_PARENT_ID\" -- TASK; \
the identity check refuses a deleted creator or a replacement with the same name. \
Your final reply reaches whoever waits on your turn.";

/// The prompt-cache lifetime human-facing clients make a bot with: an hour
/// for Claude, on Anthropic's API and on Bedrock, where a lead or monitor that
/// rests past five minutes would otherwise send its whole context again
/// uncached. An hour's writes cost twice the input rate instead of 1.25 times,
/// and a read a tenth. The daemon keeps five minutes for Bedrock models that
/// take no hour; Responses providers have no such setting.
pub fn cache_ttl(model: &str) -> Option<&'static str> {
    let claude = model.starts_with("anthropic/") || model.starts_with("bedrock/");
    claude.then_some("1h")
}

/// What human-facing clients tell a new bot's summarizer at compaction. The daemon has
/// no such text; a bot created without any never compacts.
pub const DEFAULT_COMPACTION_INSTRUCTIONS: &str = "You are summarizing the earlier part of an agent's conversation so the agent can continue \
with the summary in place of those turns. Any earlier summary is given first; merge it with the new turns, do not restart. \
Write, in order: the goal; every rule, constraint, or preference the user stated, verbatim where wording matters; \
what is done, in progress, and blocked; key decisions and why; files read or changed; open questions; next steps. \
Keep exact names, paths, commands, values, and error text. Omit chatter, repeated tool output, and anything superseded. \
Reply with the summary only.";

/// Where a new bot's tool outputs cut for the model go whole by default:
/// the outputs folder beside its daemon's store.
pub fn outputs_beside(store: &Path) -> Option<String> {
    let store = std::path::absolute(store).ok()?;
    Some(store.parent()?.join("outputs").to_str()?.to_owned())
}

/// [`outputs_beside`] the store a client reaches by default:
/// `AGENT_STORE`, else `~/.agent/state.sqlite`.
pub fn default_outputs() -> Option<String> {
    let store = match std::env::var_os("AGENT_STORE") {
        Some(store) => PathBuf::from(store),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".agent/state.sqlite"),
    };
    outputs_beside(&store)
}

/// One instruction file that went into the text, for the client to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub path: PathBuf,
    pub bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instructions {
    pub text: String,
    pub sources: Vec<Source>,
    /// The memory indexes that went into the text; one that does not
    /// exist is not listed.
    pub memory: Vec<Source>,
    pub skills: Vec<Skill>,
    pub profiles: Vec<Entry>,
}

/// Why the text could not be composed. Both are reported, never worked
/// around: a bot created without a rule it should have had is worse than
/// no bot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    TooLong {
        path: PathBuf,
        total: usize,
    },
    /// A skills or profiles folder with more entries than an index visits.
    TooMany {
        path: PathBuf,
    },
    Unreadable {
        path: PathBuf,
        reason: String,
    },
    /// A profile or skill another names (or the one asked for) is in
    /// neither folder. `kind` is `profile` or `skill`.
    Missing {
        name: String,
        kind: &'static str,
    },
    /// A profile whose `extends` or `skills` cannot be followed.
    Invalid {
        path: PathBuf,
        reason: String,
    },
}
impl Failure {
    /// A stable code for programs, in the daemon's error style.
    pub fn code(&self) -> &'static str {
        match self {
            Failure::TooLong { .. } | Failure::TooMany { .. } => "instructions_limit",
            Failure::Unreadable { .. } => "instructions_unreadable",
            Failure::Missing { kind: "skill", .. } => "skill_not_found",
            Failure::Missing { .. } => "profile_not_found",
            Failure::Invalid { .. } => "profile_invalid",
        }
    }
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::TooLong { path, total } => write!(
                f,
                "instructions would be {total} bytes with {}, above {MAX_INSTRUCTIONS}",
                path.display()
            ),
            Failure::TooMany { path } => write!(
                f,
                "more than {MAX_ENTRIES} entries in the skills or profiles folders, at {}",
                path.display()
            ),
            Failure::Unreadable { path, reason } => {
                write!(f, "cannot read {}: {reason}", path.display())
            }
            Failure::Missing {
                name,
                kind: "skill",
            } => write!(
                f,
                "no .agents/skills/{name}/SKILL.md in the workspace or ~/.agents/skills"
            ),
            Failure::Missing { name, .. } => write!(
                f,
                "no .agents/agents/{name}.md in the workspace or ~/.agents/agents"
            ),
            Failure::Invalid { path, reason } => write!(f, "{}: {reason}", path.display()),
        }
    }
}

fn home() -> Option<PathBuf> {
    // This crate's tests never read the person's own skills, roles or memory.
    if cfg!(test) {
        return Some(std::env::temp_dir().join("agent-client-tests-home"));
    }
    std::env::var_os("HOME").map(PathBuf::from)
}

/// The first `limit + 1` bytes at most: enough to know whether a file
/// fits in `limit`. Nothing beyond that is ever read, so a runaway file
/// fails before it fills memory.
fn read_head(path: &Path, limit: usize) -> Result<Vec<u8>, Failure> {
    use std::io::Read;
    let unreadable = |error: std::io::Error| Failure::Unreadable {
        path: path.to_path_buf(),
        reason: error.to_string(),
    };
    let file = std::fs::File::open(path).map_err(unreadable)?;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(unreadable)?;
    Ok(bytes)
}

/// A whole file of at most `limit` bytes, or `None` when it holds more.
fn read_bounded(path: &Path, limit: usize) -> Result<Option<String>, Failure> {
    let bytes = read_head(path, limit)?;
    if bytes.len() > limit {
        return Ok(None);
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|e| Failure::Unreadable {
            path: path.to_path_buf(),
            reason: e.utf8_error().to_string(),
        })
}

/// The head of a skill or profile file: at most this much is read to index it.
const SKILL_HEAD: usize = 4096;
/// Directory entries one index visits, indexed or not, so a folder full of
/// other files cannot make every composition slow. A row costs more than 15
/// bytes, so the byte budget ends a full index well before this.
const MAX_ENTRIES: usize = 4096;

/// AGENTS.md files that apply to `workspace`: the global one first, then
/// from the filesystem root down to the workspace, so the nearest file is
/// read last and wins where they disagree. Each folder contributes its
/// `AGENTS.md` and then its `.agents/AGENTS.md`. A file reached twice, such
/// as the home folder's `.agents/AGENTS.md` (the global one) or a link to
/// its folder's `AGENTS.md`, is read once, at its first place.
pub fn agents_files(workspace: &Path) -> Result<Vec<PathBuf>, Failure> {
    agents_files_from(workspace, home().as_deref())
}

fn agents_files_from(workspace: &Path, home: Option<&Path>) -> Result<Vec<PathBuf>, Failure> {
    let mut chain = Vec::new();
    let start = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    for dir in start.ancestors() {
        // Read in reverse: the global file, then from the root down, each
        // folder's `AGENTS.md` before its `.agents/AGENTS.md`.
        chain.push(dir.join(".agents").join("AGENTS.md"));
        chain.push(dir.join("AGENTS.md"));
    }
    if let Some(home) = home {
        chain.push(home.join(".agents").join("AGENTS.md"));
    }
    let mut files = Vec::new();
    let mut seen = Vec::new();
    for file in chain.into_iter().rev() {
        if !is_file(&file)? {
            continue;
        }
        let real = std::fs::canonicalize(&file).unwrap_or_else(|_| file.clone());
        if !seen.contains(&real) {
            seen.push(real);
            files.push(file);
        }
    }
    Ok(files)
}

/// The fields a skill or profile file may declare in YAML front matter
/// between `---` lines. Only these are read; any other key is ignored, so
/// files written for other harnesses load as they are.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Front {
    pub name: Option<String>,
    pub description: Option<String>,
    pub model: Option<String>,
    pub tools: Option<Vec<String>>,
    /// A profile this one builds on: its body comes first, and its model,
    /// tools and skills stand where this one names none.
    pub extends: Option<String>,
    /// Skills whose whole text a profile carries, as a Claude Code
    /// subagent's `skills` preloads them.
    pub skills: Option<Vec<String>>,
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    for q in ['"', '\''] {
        if value.len() >= 2 && value.starts_with(q) && value.ends_with(q) {
            return value[1..value.len() - 1].to_owned();
        }
    }
    value.to_owned()
}

/// `value` without a YAML comment: a `#` at its start or after whitespace,
/// outside quotes.
fn uncomment(value: &str) -> &str {
    let mut quote = None;
    let mut after_space = true;
    for (i, c) in value.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '#' && after_space => return value[..i].trim_end(),
            None if c == '"' || c == '\'' => quote = Some(c),
            None => {}
        }
        after_space = c.is_whitespace();
    }
    value
}

/// Split front matter from the body, reading the subset of YAML agent files
/// use. Scalars are one line or a `|`/`>` block below the key, joined by
/// spaces; `tools` and `skills` are a comma-separated line, a `[a, b]` flow
/// list, or `- a` items below it. Comments are dropped.
pub fn front_matter(text: &str) -> (Front, &str) {
    let mut front = Front::default();
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return (front, text);
    };
    let (block, body) = match rest.find("\n---") {
        Some(end) => {
            let after = &rest[end + 4..];
            (&rest[..end], after.split_once('\n').map_or("", |(_, b)| b))
        }
        // Unclosed: the whole head is front matter; a cut head has no body.
        None => (rest, ""),
    };
    // The list whose `- a` items the lines below its key hold.
    let mut in_list: Option<&mut Option<Vec<String>>> = None;
    let (mut tools, mut skills) = (None, None);
    let mut lines = block.lines().peekable();
    while let Some(line) = lines.next() {
        if uncomment(line).trim().is_empty() {
            continue;
        }
        if let Some(list) = in_list.as_mut()
            && let Some(item) = line.trim_start().strip_prefix("- ")
        {
            list.get_or_insert_with(Vec::new)
                .push(unquote(uncomment(item)));
            continue;
        }
        in_list = None;
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let value = uncomment(value.trim());
        let scalar = |lines: &mut std::iter::Peekable<std::str::Lines>| {
            if !value.starts_with(['|', '>']) {
                return unquote(value);
            }
            let mut text = Vec::new();
            while let Some(next) =
                lines.next_if(|l| l.trim().is_empty() || l.starts_with(char::is_whitespace))
            {
                if !next.trim().is_empty() {
                    text.push(next.trim());
                }
            }
            text.join(" ")
        };
        let list = match key.trim() {
            "name" => {
                front.name = Some(scalar(&mut lines));
                continue;
            }
            "description" => {
                front.description = Some(scalar(&mut lines));
                continue;
            }
            "model" => {
                front.model = Some(scalar(&mut lines));
                continue;
            }
            "extends" => {
                front.extends = Some(scalar(&mut lines));
                continue;
            }
            "tools" => &mut tools,
            "skills" => &mut skills,
            _ => continue,
        };
        if value.is_empty() {
            *list = Some(Vec::new());
            in_list = Some(list);
        } else {
            let items = value.trim_start_matches('[').trim_end_matches(']');
            *list = Some(
                items
                    .split(',')
                    .map(unquote)
                    .filter(|t| !t.is_empty())
                    .collect(),
            );
        }
    }
    front.tools = tools;
    front.skills = skills;
    (front, body)
}

/// An entry of the skills or profiles index: a file the bot can open, or a
/// role it can start a peer in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    /// The `description` in its front matter, or its first non-empty line,
    /// stripped of heading marks.
    pub summary: String,
}
pub type Skill = Entry;

/// Roles a client starts its own bots in: Sonny's lead, and the task
/// text Sonny tells every other agent it starts. A `.agents/agents` file
/// of one of these names replaces the client's text for that role, so it
/// is not a role to start a peer in, and the Profiles index leaves it out,
/// as it leaves out a profile that `extends` one: a variant a lead or its
/// tasks are set to run as.
pub const CLIENT_ROLES: [&str; 2] = ["lead", "task"];

/// Whether a profile file's stem is a client role. Case is ignored: on a
/// case-insensitive filesystem (macOS's default) `Lead.md` is the
/// file `profile(_, "lead")` reads.
fn client_role(stem: &str) -> bool {
    CLIENT_ROLES
        .iter()
        .any(|role| role.eq_ignore_ascii_case(stem))
}

/// Skills are folders `<name>/SKILL.md` in `<workspace>/.agents/skills`, then
/// `~/.agents/skills`, the layout agentskills.io describes. Profiles are
/// `<name>.md` files in `.agents/agents` in the same two places. The
/// workspace's wins on a clash.
#[derive(Clone, Copy)]
enum Kind {
    Skills,
    Profiles,
}
impl Kind {
    fn dir(self) -> &'static str {
        match self {
            Kind::Skills => "skills",
            Kind::Profiles => "agents",
        }
    }
    fn header(self) -> String {
        match self {
            Kind::Skills => {
                "\n\n# Skills\n\nRead a skill file with the read tool when its subject comes up.\n"
                    .into()
            }
            Kind::Profiles => format!(
                "\n\n# Profiles\n\nStart an agent in one of these roles with {} run --detach --new --profile ROLE --bot NAME -- TASK.\n",
                cli()
            ),
        }
    }
    /// The name an entry of this directory would index under. Only a name
    /// --profile accepts is offered as a role, and never a client's own.
    fn name(self, path: &Path) -> Option<String> {
        let name = path.file_name()?.to_str()?;
        match self {
            Kind::Skills => Some(name.to_owned()),
            Kind::Profiles => name
                .strip_suffix(".md")
                .filter(|stem| profile_name(stem) && !client_role(stem))
                .map(str::to_owned),
        }
    }
    /// The file the entry stands for, if it is one.
    fn file(self, path: &Path) -> Result<Option<PathBuf>, Failure> {
        let file = match self {
            Kind::Skills => path.join("SKILL.md"),
            Kind::Profiles => path.to_path_buf(),
        };
        // The index names what the folders hold; whether a role fits a bot's
        // instructions is known only when it is composed, and --profile then
        // fails with instructions_limit rather than cutting it.
        Ok(is_file(&file)?.then_some(file))
    }
}

/// Whether `path` is a file. Only an absent path is not one; any other error
/// is reported, so an unreadable workspace file never lets the user's file of
/// the same name stand in for it.
fn is_file(path: &Path) -> Result<bool, Failure> {
    match std::fs::metadata(path) {
        Ok(meta) => Ok(meta.is_file()),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            not_dangling(path)?;
            Ok(false)
        }
        Err(error) => Err(Failure::Unreadable {
            path: path.to_path_buf(),
            reason: error.to_string(),
        }),
    }
}

/// Called when `path` does not resolve. A link to a missing target, at
/// `path` or at the nearest part of it that exists (a dangling `.agents` or
/// skill folder), is present, so it is an error, not an absence another
/// folder's file may stand in for. Usually one extra lookup: the parent.
fn not_dangling(path: &Path) -> Result<(), Failure> {
    for part in path.ancestors() {
        let Ok(meta) = std::fs::symlink_metadata(part) else {
            continue;
        };
        if meta.file_type().is_symlink() && std::fs::metadata(part).is_err() {
            return Err(Failure::Unreadable {
                path: part.to_path_buf(),
                reason: "a link to a missing file".into(),
            });
        }
        return Ok(());
    }
    Ok(())
}

/// The memory indexes a new bot in `workspace` starts with: the person's
/// `~/.agents/memory/MEMORY.md`, then, when the workspace is in a project,
/// `~/.agents/memory/projects/NAME/MEMORY.md`. The app's memory script
/// keeps both.
pub fn memory_indexes(workspace: &Path) -> Result<Vec<PathBuf>, Failure> {
    memory_indexes_from(workspace, home().as_deref())
}

fn memory_indexes_from(workspace: &Path, home: Option<&Path>) -> Result<Vec<PathBuf>, Failure> {
    let Some(home) = home else {
        return Ok(Vec::new());
    };
    let project = project_from(workspace, Some(home))?;
    Ok(indexes_of(
        project.map(|(name, _)| name).as_deref(),
        Some(home),
    ))
}

/// The memory indexes a new bot of the project named `project` starts
/// with, as [`memory_indexes`] finds them for a folder in it: for a
/// project's lead, whose folder has no project file until it is made, and
/// would otherwise take the memory of a project above it.
pub fn project_memory_indexes(project: &str) -> Vec<PathBuf> {
    indexes_of(Some(project), home().as_deref())
}

fn indexes_of(project: Option<&str>, home: Option<&Path>) -> Vec<PathBuf> {
    let Some(home) = home else {
        return Vec::new();
    };
    let root = home.join(".agents/memory");
    let mut found = vec![root.join("MEMORY.md")];
    if let Some(name) = project {
        found.push(root.join("projects").join(name).join("MEMORY.md"));
    }
    found
}

/// The project `dir` belongs to: the name in the nearest
/// `.agents/project.toml` at or above it, looking no higher than its
/// repository's top, and taking the home folder's only for the home folder
/// itself: a repository, or a folder, under another project's is a project
/// of its own. A task's worktree is another checkout of its project's
/// repository, where that file may not be committed or may differ, so a
/// folder in one is looked up at the same place in the repository's main
/// checkout first.
pub fn project(dir: &Path) -> Result<Option<String>, Failure> {
    Ok(project_from(dir, home().as_deref())?.map(|(name, _)| name))
}

/// The project `dir` is in, as [`project`] finds it, and the folder whose
/// `.agents/project.toml` names it: the one its coordinator works in.
pub fn project_root(dir: &Path) -> Result<Option<(String, PathBuf)>, Failure> {
    project_from(dir, home().as_deref())
}

fn project_from(dir: &Path, home: Option<&Path>) -> Result<Option<(String, PathBuf)>, Failure> {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let home = home.map(|h| h.canonicalize().unwrap_or_else(|_| h.to_path_buf()));
    let main = main_checkout(&dir);
    for start in main.iter().chain(std::iter::once(&dir)) {
        for at in start.ancestors() {
            if home.as_deref() == Some(at) && at != start.as_path() {
                break;
            }
            let path = at.join(".agents/project.toml");
            if !is_file(&path)? {
                if std::fs::symlink_metadata(at.join(".git")).is_ok() {
                    break;
                }
                continue;
            }
            let unreadable = |reason: String| Failure::Unreadable {
                path: path.clone(),
                reason,
            };
            let text = read_bounded(&path, 64 * 1024)?
                .ok_or_else(|| unreadable("larger than 64 KiB".into()))?;
            let table: toml::Table = text
                .parse()
                .map_err(|e: toml::de::Error| unreadable(e.message().to_owned()))?;
            return match table.get("name") {
                Some(toml::Value::String(name)) if project_name(name) => {
                    Ok(Some((name.clone(), at.to_path_buf())))
                }
                _ => Err(unreadable(
                    "name must be 1-64 of A-Z a-z 0-9 - _ ., not starting or ending with .".into(),
                )),
            };
        }
    }
    Ok(None)
}

/// The top of the git checkout `dir` is in, as the main checkout's when it
/// is a linked worktree, looking no higher than the home folder (the home
/// folder's own only for the home folder itself), as [`project`] looks.
pub fn checkout(dir: &Path) -> Option<PathBuf> {
    checkout_from(dir, home().as_deref())
}

fn checkout_from(dir: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let home = home.map(|h| h.canonicalize().unwrap_or_else(|_| h.to_path_buf()));
    for at in dir.ancestors() {
        if home.as_deref() == Some(at) && at != dir.as_path() {
            return None;
        }
        if std::fs::symlink_metadata(at.join(".git")).is_ok() {
            return Some(main_checkout(at).unwrap_or_else(|| at.to_path_buf()));
        }
    }
    None
}

fn project_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        && !name.starts_with('.')
        && !name.ends_with('.')
}

/// The same place as `dir` in its repository's main checkout, when `dir` is
/// in a linked worktree: its `.git` is a file naming the worktree's git
/// folder, whose `commondir` names the main checkout's `.git`. Read from
/// the files rather than by running git, so composing spawns nothing.
fn main_checkout(dir: &Path) -> Option<PathBuf> {
    for top in dir.ancestors() {
        let dot = top.join(".git");
        let Ok(meta) = std::fs::symlink_metadata(&dot) else {
            continue;
        };
        if !meta.is_file() {
            return None;
        }
        let text = std::fs::read_to_string(&dot).ok()?;
        let gitdir = top.join(text.strip_prefix("gitdir: ")?.trim());
        let common = std::fs::read_to_string(gitdir.join("commondir")).ok()?;
        let common = gitdir.join(common.trim()).canonicalize().ok()?;
        if common.file_name()? != ".git" {
            return None;
        }
        return Some(common.parent()?.join(dir.strip_prefix(top).ok()?));
    }
    None
}

fn entry_row(entry: &Entry) -> String {
    format!(
        "\n- {}: {} ({})",
        entry.name,
        entry.summary,
        entry.path.display()
    )
}

/// How short each summary may become, in characters, before entries are
/// left out: Codex trims its skill descriptions before it drops skills.
const SUMMARY_STEPS: [usize; 4] = [160, 80, 40, 0];

/// A list of `entries` within `room` bytes, with the entries it lists:
/// whole when it fits; else each summary shortened, step by step; else
/// without summaries and without the last entries by name, with a line
/// saying how many were left out. Empty when not even that fits.
fn fit(kind: Kind, entries: Vec<Entry>, room: usize) -> (String, Vec<Entry>) {
    if entries.is_empty() {
        return (String::new(), entries);
    }
    let header = kind.header();
    let block = |entries: &[Entry], chars: usize| {
        let mut block = header.clone();
        for entry in entries {
            block.push_str(&entry_row(&Entry {
                summary: entry.summary.chars().take(chars).collect(),
                ..entry.clone()
            }));
        }
        block
    };
    for chars in SUMMARY_STEPS {
        let whole = block(&entries, chars);
        if whole.len() <= room {
            let kept = (entries.into_iter())
                .map(|entry| Entry {
                    summary: entry.summary.chars().take(chars).collect(),
                    ..entry
                })
                .collect();
            return (whole, kept);
        }
    }
    let total = entries.len();
    let mut kept = entries;
    for entry in &mut kept {
        entry.summary.clear();
    }
    let note = |left: usize| {
        format!("\n- {left} more not listed, for room; list the folders above to see them all.")
    };
    let mut listed = block(&kept, 0);
    while !kept.is_empty() && listed.len() + note(total - kept.len()).len() > room {
        let gone = kept.pop().expect("an entry");
        listed.truncate(listed.len() - entry_row(&gone).len());
    }
    if kept.is_empty() {
        return (String::new(), kept);
    }
    listed.push_str(&note(total - kept.len()));
    (listed, kept)
}

fn search(workspace: &Path, kind: Kind) -> Vec<PathBuf> {
    // Visit the winning directory first, so a later directory can only add
    // entries. A budget failure can never be undone by an override.
    let mut dirs = vec![workspace.join(".agents").join(kind.dir())];
    // A workspace that is the home folder is searched once.
    if let Some(h) = home()
        && !matches!(
            (std::fs::canonicalize(&h), std::fs::canonicalize(workspace)),
            (Ok(a), Ok(b)) if a == b
        )
    {
        dirs.push(h.join(".agents").join(kind.dir()));
    }
    dirs
}

fn index(dirs: Vec<PathBuf>, kind: Kind, budget: usize) -> Result<Vec<Entry>, Failure> {
    let mut found = std::collections::BTreeMap::<String, Entry>::new();
    // Profiles that build on a client role: that role's variants, set as
    // a lead's or task's role, not roles to start a peer in.
    let mut hidden = std::collections::BTreeSet::<String>::new();
    // What each indexed profile extends, to hide variants of variants.
    let mut bases = std::collections::BTreeMap::<String, String>::new();
    let mut used = 0usize;
    let mut visited = 0usize;
    for dir in dirs {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                not_dangling(&dir)?;
                continue;
            }
            Err(error) => {
                return Err(Failure::Unreadable {
                    path: dir,
                    reason: error.to_string(),
                });
            }
        };
        // Do not collect directory contents: the index must fit before any
        // more paths or file heads are read. Final ordering is bounded above.
        for entry in entries {
            visited += 1;
            if visited > MAX_ENTRIES {
                return Err(Failure::TooMany { path: dir });
            }
            let path = entry
                .map_err(|error| Failure::Unreadable {
                    path: dir.clone(),
                    reason: error.to_string(),
                })?
                .path();
            // A shadowed entry is skipped before its file is probed, so an
            // override can never be undone by the file it overrides.
            let Some(name) = kind.name(&path) else {
                continue;
            };
            if found.contains_key(&name) || hidden.contains(&name) {
                continue;
            }
            let Some(file) = kind.file(&path)? else {
                continue;
            };
            let mut entry = Entry {
                name,
                path: file,
                summary: String::new(),
            };
            let too_long = |size| Failure::TooLong {
                path: entry.path.clone(),
                total: MAX_INSTRUCTIONS.saturating_sub(budget) + size,
            };
            let minimum = used + entry_row(&entry).len();
            if minimum > budget {
                return Err(too_long(minimum));
            }
            let mut bytes = read_head(&entry.path, SKILL_HEAD)?;
            bytes.truncate(SKILL_HEAD);
            let text = match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(error) if error.utf8_error().error_len().is_none() => {
                    let valid = error.utf8_error().valid_up_to();
                    String::from_utf8_lossy(&error.into_bytes()[..valid]).into_owned()
                }
                Err(error) => {
                    return Err(Failure::Unreadable {
                        path: entry.path,
                        reason: error.utf8_error().to_string(),
                    });
                }
            };
            let (front, body) = front_matter(&text);
            if matches!(kind, Kind::Profiles) && front.extends.as_deref().is_some_and(client_role) {
                hidden.insert(entry.name);
                continue;
            }
            if let Some(base) = front.extends.filter(|b| *b != entry.name) {
                bases.insert(entry.name.clone(), base);
            }
            let summary = front.description.filter(|d| !d.is_empty()).or_else(|| {
                body.lines()
                    .map(|l| l.trim().trim_start_matches('#').trim())
                    .find(|l| !l.is_empty())
                    .map(str::to_owned)
            });
            entry.summary = summary
                .map(|s| s.chars().take(160).collect())
                .unwrap_or_default();
            used += entry_row(&entry).len();
            if used > budget {
                return Err(too_long(used));
            }
            found.insert(entry.name.clone(), entry);
        }
    }
    // A profile that extends a hidden one is a client role's variant too.
    loop {
        let more: Vec<String> = (bases.iter())
            .filter(|(name, base)| found.contains_key(*name) && hidden.contains(*base))
            .map(|(name, _)| name.clone())
            .collect();
        if more.is_empty() {
            break;
        }
        for name in more {
            found.remove(&name);
            hidden.insert(name);
        }
    }
    Ok(found.into_values().collect())
}

/// A role a bot is started in: the body of `.agents/agents/<name>.md`, and
/// the model and tools its front matter names, if any. A profile that
/// `extends` another has that one's body first and its model and tools
/// where it names none; the skills either names follow, each whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    /// None for a role a client ships built in.
    pub path: Option<PathBuf>,
    pub model: Option<String>,
    pub tools: Option<Vec<String>>,
    pub body: String,
}
impl Profile {
    /// One file's role as it reads, its `extends` and `skills` not followed.
    pub fn parse(name: &str, path: Option<PathBuf>, text: &str) -> Profile {
        let (front, body) = front_matter(text);
        Profile {
            name: name.to_owned(),
            path,
            model: front.model.filter(|m| !m.is_empty()),
            tools: front.tools,
            body: body.trim().to_owned(),
        }
    }
}

/// A profile name is one file name, nothing that could reach another folder,
/// and a word a shell passes unquoted.
pub fn profile_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// The most profiles one role is made of, itself included.
const MAX_EXTENDS: usize = 8;

/// The profile named `name` for a bot in `workspace`: the workspace's own
/// file, else the user's; `None` when neither exists.
pub fn profile(workspace: &Path, name: &str) -> Result<Option<Profile>, Failure> {
    profile_with(workspace, name, &[])
}

/// [`profile`], with the roles a client ships (`(name, text)`) after the
/// person's: one of these names is never `None`.
///
/// A profile builds on another with `extends: NAME` in its front matter.
/// Extending its own name builds on the file of that name it shadows: a
/// folder's `lead.md` on the person's, the person's on the client's.
/// `skills: [a, b]` adds each skill's whole text after the body.
pub fn profile_with(
    workspace: &Path,
    name: &str,
    builtins: &[(&str, &str)],
) -> Result<Option<Profile>, Failure> {
    let mut layers: Vec<Option<PathBuf>> = search(workspace, Kind::Profiles)
        .into_iter()
        .map(Some)
        .collect();
    // The client's own roles: the layer below the person's.
    layers.push(None);
    let mut chain: Vec<(Profile, Front)> = Vec::new();
    let mut seen: Vec<(String, usize)> = Vec::new();
    // What the role's files hold so far: none is read past what could
    // still fit in a bot's instructions.
    let mut used = 0usize;
    let (mut want, mut from) = (name.to_owned(), 0);
    loop {
        if !profile_name(&want) {
            return Err(Failure::Invalid {
                path: chain
                    .last()
                    .and_then(|(p, _)| p.path.clone())
                    .unwrap_or_else(|| want.clone().into()),
                reason: format!(
                    "{want:?} is not a profile name: letters, digits, '-', '_' and '.'"
                ),
            });
        }
        let room = MAX_INSTRUCTIONS.saturating_sub(used);
        let Some((layer, profile, front)) = find(&layers, &want, from, builtins, room)? else {
            if chain.is_empty() {
                return Ok(None);
            }
            return Err(Failure::Missing {
                name: want,
                kind: "profile",
            });
        };
        if seen.contains(&(want.clone(), layer)) || chain.len() == MAX_EXTENDS {
            let mut names: Vec<_> = chain.iter().map(|(p, _)| p.name.as_str()).collect();
            names.push(&want);
            return Err(Failure::Invalid {
                path: profile.path.clone().unwrap_or_else(|| want.clone().into()),
                reason: format!(
                    "extends goes round or deeper than {MAX_EXTENDS}: {}",
                    names.join(" -> ")
                ),
            });
        }
        seen.push((want.clone(), layer));
        used += profile.body.len();
        let base = front.extends.clone().filter(|b| !b.is_empty());
        chain.push((profile, front));
        match base {
            None => break,
            // Its own name: the file it shadows.
            Some(base) if base == want => from = layer + 1,
            Some(base) => (want, from) = (base, 0),
        }
    }
    // The base first: each profile adds to what it extends.
    let mut role = chain[0].0.clone();
    role.name = name.to_owned();
    role.model = None;
    role.tools = None;
    let mut bodies = Vec::new();
    let mut skills: Vec<String> = Vec::new();
    for (profile, front) in chain.iter().rev() {
        if !profile.body.is_empty() {
            bodies.push(profile.body.clone());
        }
        role.model = profile.model.clone().or(role.model);
        role.tools = profile.tools.clone().or(role.tools);
        for skill in front.skills.iter().flatten() {
            if !skills.contains(skill) {
                skills.push(skill.clone());
            }
        }
    }
    for skill in &skills {
        let text = skill_text(workspace, skill, MAX_INSTRUCTIONS.saturating_sub(used))?;
        used += text.len();
        bodies.push(format!("## Skill: {skill}\n\n{text}"));
    }
    role.body = bodies.join("\n\n");
    Ok(Some(role))
}

/// The first profile named `name` at `from` or a later layer, with the layer
/// it is in and its front matter.
fn find(
    layers: &[Option<PathBuf>],
    name: &str,
    from: usize,
    builtins: &[(&str, &str)],
    room: usize,
) -> Result<Option<(usize, Profile, Front)>, Failure> {
    for (at, layer) in layers.iter().enumerate().skip(from) {
        let Some(dir) = layer else {
            let Some((_, text)) = builtins.iter().find(|(n, _)| *n == name) else {
                continue;
            };
            return Ok(Some((
                at,
                Profile::parse(name, None, text),
                front_matter(text).0,
            )));
        };
        let path = dir.join(format!("{name}.md"));
        if !is_file(&path)? {
            continue;
        }
        let text = role_file(&path, room)?;
        let front = front_matter(&text).0;
        return Ok(Some((at, Profile::parse(name, Some(path), &text), front)));
    }
    Ok(None)
}

/// A profile's or skill's whole file, if it fits in `room`. An unclosed
/// head would read the whole file as front matter and start the bot
/// without its text.
fn role_file(path: &Path, room: usize) -> Result<String, Failure> {
    let Some(text) = read_bounded(path, room)? else {
        return Err(Failure::TooLong {
            total: MAX_INSTRUCTIONS - room
                + std::fs::metadata(path).map_or(room + 1, |m| m.len() as usize),
            path: path.to_path_buf(),
        });
    };
    if (text.starts_with("---\n") || text.starts_with("---\r\n")) && !text[3..].contains("\n---") {
        return Err(Failure::Unreadable {
            path: path.to_path_buf(),
            reason: "front matter opened with --- is not closed".into(),
        });
    }
    Ok(text)
}

/// The body of the skill `name` a profile carries, if it fits in `room`:
/// the workspace's, else the user's.
fn skill_text(workspace: &Path, name: &str, room: usize) -> Result<String, Failure> {
    if !profile_name(name) {
        return Err(Failure::Invalid {
            path: name.into(),
            reason: format!("{name:?} is not a skill name"),
        });
    }
    for dir in search(workspace, Kind::Skills) {
        let path = dir.join(name).join("SKILL.md");
        if !is_file(&path)? {
            continue;
        }
        let text = role_file(&path, room)?;
        return Ok(front_matter(&text).1.trim().to_owned());
    }
    Err(Failure::Missing {
        name: name.to_owned(),
        kind: "skill",
    })
}

/// Append `path` to `text` under `header`, unless it is empty. Reads no
/// more than what could still fit, so a file past the budget fails on its
/// size, not after being copied into memory.
fn append(text: &mut String, path: PathBuf, header: &str) -> Result<Option<Source>, Failure> {
    let room = MAX_INSTRUCTIONS.saturating_sub(text.len() + header.len());
    let Some(body) = read_bounded(&path, room)? else {
        let size = std::fs::metadata(&path).map_or(room + 1, |m| m.len() as usize);
        return Err(Failure::TooLong {
            path,
            total: text.len() + header.len() + size,
        });
    };
    let body = body.trim();
    if body.is_empty() {
        return Ok(None);
    }
    let block = format!("{header}{body}");
    if text.len() + block.len() > MAX_INSTRUCTIONS {
        return Err(Failure::TooLong {
            path,
            total: text.len() + block.len(),
        });
    }
    text.push_str(&block);
    Ok(Some(Source {
        path,
        bytes: body.len(),
    }))
}

/// What heads an AGENTS.md file, and a role, in a bot's instructions.
const AGENTS_HEADING: &str = "\n\n# Instructions from ";
const ROLE_HEADING: &str = "\n\n# Role: ";

fn agents_header(path: &Path) -> String {
    format!("{AGENTS_HEADING}{}\n\n", path.display())
}

/// The AGENTS.md files that apply to `workspace`, with their text, when
/// `instructions`, a bot's, hold them all as they read now: what the bot
/// was given, unchanged since. The files must be the whole of what it was
/// given, with no file before them and, right after the last, the end or
/// a heading only composing writes, so a file cut short, added, or removed
/// since reads as changed, and then none is returned. Memory never marks
/// that end: agents write it, so an index could be made to read as the
/// paragraphs cut from a file. Where memory is all that follows, none is
/// returned.
pub fn given_agents_files(workspace: &Path, instructions: &str) -> Vec<(PathBuf, String)> {
    let Ok(paths) = agents_files(workspace) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    let mut blocks = String::new();
    for path in paths {
        let Ok(Some(body)) = read_bounded(&path, MAX_INSTRUCTIONS) else {
            return Vec::new();
        };
        let body = body.trim();
        // An empty file is given no section.
        if body.is_empty() {
            continue;
        }
        blocks.push_str(&agents_header(&path));
        blocks.push_str(body);
        files.push((path, body.to_owned()));
    }
    if files.is_empty() {
        return files;
    }
    // What the instructions compose after the files: the skills or
    // profiles list, or the role, which comes last, so no list follows it.
    let lists = [Kind::Skills.header(), Kind::Profiles.header()];
    let follows = |rest: &str| {
        rest.is_empty()
            || lists.iter().any(|header| rest.starts_with(header.as_str()))
            || (rest.starts_with(ROLE_HEADING)
                && !lists.iter().any(|header| rest.contains(header.as_str())))
    };
    let given = instructions.match_indices(&blocks).any(|(at, _)| {
        !instructions[..at].contains(AGENTS_HEADING) && follows(&instructions[at + blocks.len()..])
    });
    match given {
        true => files,
        false => Vec::new(),
    }
}

/// The full text for a new bot in `workspace`, in `role` when given, with
/// the `memory` indexes the client keeps. Fails rather than truncates when
/// the files do not fit: a silently shortened AGENTS.md is worse than none.
pub fn instructions(
    workspace: &Path,
    role: Option<&Profile>,
    memory: &[PathBuf],
) -> Result<Instructions, Failure> {
    let mut text = preamble();
    let mut sources = Vec::new();
    for path in agents_files(workspace)? {
        let header = agents_header(&path);
        sources.extend(append(&mut text, path, &header)?);
    }
    // Memory and the role go whole, so they are composed first, and the
    // skill and profile lists take the room they leave: shortened, then
    // cut, rather than failing, as Codex and Claude Code size their skill
    // lists. The text keeps its order: files, lists, memory, role.
    let mut after = text.clone();
    // Memory follows: what earlier agents saved, as it stands when the bot
    // is made. An index carries its own heading, so it goes in as it is;
    // one not written yet is none.
    let mut indexes = Vec::new();
    for path in memory {
        match std::fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => not_dangling(path)?,
            _ => indexes.extend(append(&mut after, path.clone(), "\n\n")?),
        }
    }
    // The role comes last: the most specific text a bot is given.
    if let Some(role) = role.filter(|r| !r.body.is_empty()) {
        let block = format!("{ROLE_HEADING}{}\n\n{}", role.name, with_cli(&role.body));
        if after.len() + block.len() > MAX_INSTRUCTIONS {
            return Err(Failure::TooLong {
                path: role
                    .path
                    .clone()
                    .unwrap_or_else(|| PathBuf::from(&role.name)),
                total: after.len() + block.len(),
            });
        }
        after.push_str(&block);
    }
    let tail = after.split_off(text.len());
    let mut lists = Vec::new();
    for kind in [Kind::Skills, Kind::Profiles] {
        let entries = index(search(workspace, kind), kind, usize::MAX)?;
        let room = MAX_INSTRUCTIONS.saturating_sub(text.len() + tail.len());
        let (block, kept) = fit(kind, entries, room);
        text.push_str(&block);
        lists.push(kept);
    }
    text.push_str(&tail);
    let profiles = lists.pop().unwrap_or_default();
    let skills = lists.pop().unwrap_or_default();
    Ok(Instructions {
        text,
        sources,
        memory: indexes,
        skills,
        profiles,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_caches_for_an_hour() {
        for model in [
            "anthropic/claude-opus-5-5",
            "bedrock/us.anthropic.claude-opus-5-5-v1:0",
        ] {
            assert_eq!(cache_ttl(model), Some("1h"), "{model}");
        }
        for model in ["openai/gpt-6", "chatgpt/gpt-6-sol"] {
            assert_eq!(cache_ttl(model), None, "{model}");
        }
    }

    #[test]
    fn a_client_that_wraps_the_cli_names_its_own_way_in() {
        let sonny = "\"/opt/homebrew/opt/sonny/bin/sonny\" agent";
        let text = naming(PREAMBLE, sonny);
        assert!(!text.contains("AGENT_BIN"), "{text}");
        assert!(text.contains(&format!("{sonny} run --detach --new --bot NAME -- TASK")));
        assert!(text.contains(&format!("{sonny} run --detach --bot \"$AGENT_PARENT\"")));
        // Without one, the binary the daemon gives every shell.
        assert_eq!(naming(PREAMBLE, "\"$AGENT_BIN\""), PREAMBLE);
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agent-policy-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Discovery reports canonical paths (macOS aliases /var to /private/var).
        std::fs::canonicalize(&dir).unwrap()
    }

    #[test]
    fn nearest_agents_md_is_read_last_and_skills_are_indexed() {
        let root = temp("chain");
        let deep = root.join("repo").join("crate");
        std::fs::create_dir_all(deep.join(".agents/skills/deploy")).unwrap();
        std::fs::create_dir_all(deep.join(".agents/skills/empty")).unwrap();
        std::fs::write(root.join("AGENTS.md"), "outer rule").unwrap();
        std::fs::write(deep.join("AGENTS.md"), "# inner\n\ninner rule").unwrap();
        std::fs::write(
            deep.join(".agents/skills/deploy/SKILL.md"),
            "# Deploy\n\nShip a release safely.",
        )
        .unwrap();
        std::fs::write(deep.join(".agents/skills/notes.md"), "not a skill").unwrap();
        let files = agents_files(&deep).unwrap();
        assert_eq!(
            files.iter().rev().take(2).collect::<Vec<_>>(),
            vec![&deep.join("AGENTS.md"), &root.join("AGENTS.md")]
        );
        let composed = instructions(&deep, None, &[]).unwrap();
        assert!(composed.text.starts_with(PREAMBLE));
        let outer = composed.text.find("outer rule").unwrap();
        let inner = composed.text.find("inner rule").unwrap();
        assert!(outer < inner, "the nearest file is read last");
        assert_eq!(
            composed.sources.iter().next_back().map(|s| s.bytes),
            Some("# inner\n\ninner rule".len())
        );
        assert_eq!(composed.skills.len(), 1);
        assert_eq!(composed.skills[0].name, "deploy");
        assert_eq!(composed.skills[0].summary, "Deploy");
        assert!(composed.text.contains("- deploy: Deploy ("));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_bot_was_given_the_agents_files_that_read_as_they_did() {
        let root = temp("given-agents");
        let deep = root.join("repo");
        std::fs::create_dir_all(&deep).unwrap();
        let outer = "Push when finished.\n\nOnly push to staging; never to production.";
        std::fs::write(root.join("AGENTS.md"), outer).unwrap();
        std::fs::write(deep.join("AGENTS.md"), "inner rule").unwrap();
        let given = instructions(&deep, None, &[]).unwrap().text;
        let found = || -> Vec<String> {
            given_agents_files(&deep, &given)
                .into_iter()
                .filter(|(path, _)| path.starts_with(&root))
                .map(|(_, text)| text)
                .collect()
        };
        assert_eq!(found(), [outer, "inner rule"]);
        // A role after them is a section composed after them.
        let role = Profile::parse("reviewer", None, "Review.");
        let with_role = instructions(&deep, Some(&role), &[]).unwrap().text;
        assert_eq!(given_agents_files(&deep, &with_role).len(), found().len());
        // Edited since: none is what the bot was given.
        std::fs::write(deep.join("AGENTS.md"), "inner rule, and push freely").unwrap();
        assert!(found().is_empty());
        std::fs::write(deep.join("AGENTS.md"), "inner rule").unwrap();
        // Nor when one is cut short at a paragraph, dropping its restriction.
        std::fs::write(root.join("AGENTS.md"), "Push when finished.").unwrap();
        assert!(found().is_empty());
        // Nor when memory, which agents write, is what follows them: an
        // index made to read as the cut paragraph would pass for it.
        let cut = "Act freely.\n\nNever push.";
        std::fs::write(root.join("AGENTS.md"), cut).unwrap();
        let cut_given = instructions(&deep, None, &[]).unwrap().text;
        std::fs::write(root.join("AGENTS.md"), "Act freely.").unwrap();
        assert!(given_agents_files(&deep, &cut_given).is_empty());
        // Nor when the cut leaves a role-like heading of the file's own
        // before a list: the role comes last.
        let headed = "Act freely.\n\n# Role: careful\n\nNever push.";
        std::fs::write(root.join("AGENTS.md"), headed).unwrap();
        std::fs::create_dir_all(deep.join(".agents/skills/deploy")).unwrap();
        std::fs::write(
            deep.join(".agents/skills/deploy/SKILL.md"),
            "# Deploy\n\nShip.",
        )
        .unwrap();
        let headed_given = instructions(&deep, None, &[]).unwrap().text;
        std::fs::write(root.join("AGENTS.md"), "Act freely.").unwrap();
        assert!(given_agents_files(&deep, &headed_given).is_empty());
        std::fs::write(root.join("AGENTS.md"), headed).unwrap();
        assert!(!given_agents_files(&deep, &headed_given).is_empty());
        std::fs::remove_dir_all(deep.join(".agents")).unwrap();
        // Nor when one is removed.
        std::fs::write(root.join("AGENTS.md"), outer).unwrap();
        assert_eq!(found().len(), 2);
        std::fs::remove_file(root.join("AGENTS.md")).unwrap();
        assert!(found().is_empty());
    }

    #[test]
    fn each_folder_adds_its_agents_folder_file_after_its_own() {
        let root = temp("dot-agents");
        let deep = root.join("repo").join("crate");
        std::fs::create_dir_all(deep.join(".agents")).unwrap();
        std::fs::create_dir_all(root.join(".agents")).unwrap();
        std::fs::write(root.join("AGENTS.md"), "outer rule").unwrap();
        std::fs::write(root.join(".agents/AGENTS.md"), "outer agents rule").unwrap();
        std::fs::write(deep.join("AGENTS.md"), "inner rule").unwrap();
        std::fs::write(deep.join(".agents/AGENTS.md"), "inner agents rule").unwrap();
        // A `.agents` that is a file holds no AGENTS.md.
        std::fs::write(root.join("repo/.agents"), "not a folder").unwrap();
        let files = agents_files_from(&deep, None).unwrap();
        assert_eq!(
            files.iter().rev().take(4).rev().collect::<Vec<_>>(),
            vec![
                &root.join("AGENTS.md"),
                &root.join(".agents/AGENTS.md"),
                &deep.join("AGENTS.md"),
                &deep.join(".agents/AGENTS.md"),
            ]
        );
        // The home folder's `.agents/AGENTS.md` is the global file: read
        // first, and not again as an ancestor's.
        let files = agents_files_from(&deep, Some(&root)).unwrap();
        assert_eq!(files[0], root.join(".agents/AGENTS.md"));
        assert_eq!(
            files
                .iter()
                .filter(|f| **f == root.join(".agents/AGENTS.md"))
                .count(),
            1
        );
        assert_eq!(files.last(), Some(&deep.join(".agents/AGENTS.md")));
        let composed = instructions(&deep, None, &[]).unwrap();
        let order = [
            "outer rule",
            "outer agents rule",
            "inner rule",
            "inner agents rule",
        ]
        .map(|rule| composed.text.find(&format!("\n\n{rule}")).unwrap());
        assert!(order.is_sorted(), "{order:?}");
        // A `.agents/AGENTS.md` that links to its folder's file is read once.
        let linked = root.join("linked");
        std::fs::create_dir_all(linked.join(".agents")).unwrap();
        std::fs::write(linked.join("AGENTS.md"), "linked rule").unwrap();
        std::os::unix::fs::symlink("../AGENTS.md", linked.join(".agents/AGENTS.md")).unwrap();
        let files = agents_files_from(&linked, None).unwrap();
        assert_eq!(files.last(), Some(&linked.join("AGENTS.md")));
        assert!(
            !files.contains(&linked.join(".agents/AGENTS.md")),
            "{files:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn oversized_files_fail_instead_of_being_cut() {
        let root = temp("big");
        std::fs::write(root.join("AGENTS.md"), "x".repeat(MAX_INSTRUCTIONS)).unwrap();
        let error = instructions(&root, None, &[]).unwrap_err();
        assert_eq!(error.code(), "instructions_limit");
        assert!(
            matches!(&error, Failure::TooLong { path, total } if *path == root.join("AGENTS.md") && *total > MAX_INSTRUCTIONS)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_runaway_file_fails_on_its_size_without_being_read_whole() {
        let root = temp("runaway");
        // Sparse: the file is huge on disk, cheap to create, and reading
        // it whole would allocate it all.
        let file = std::fs::File::create(root.join("AGENTS.md")).unwrap();
        file.set_len(512 * 1024 * 1024).unwrap();
        drop(file);
        let error = instructions(&root, None, &[]).unwrap_err();
        assert!(
            matches!(&error, Failure::TooLong { path, total } if *path == root.join("AGENTS.md") && *total > 512 * 1024 * 1024)
        );
        std::fs::create_dir_all(root.join(".agents/skills/big")).unwrap();
        std::fs::write(root.join("AGENTS.md"), "rule").unwrap();
        let mut long = String::from("# Big skill\n\n");
        long.push_str(&"x".repeat(SKILL_HEAD * 4));
        std::fs::write(root.join(".agents/skills/big/SKILL.md"), long).unwrap();
        let composed = instructions(&root, None, &[]).unwrap();
        assert_eq!(composed.skills[0].summary, "Big skill");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unreadable_file_is_an_error_not_a_silent_omission() {
        let root = temp("unreadable");
        std::fs::write(root.join("AGENTS.md"), [0xff, 0xfe, b'x']).unwrap();
        let error = instructions(&root, None, &[]).unwrap_err();
        assert_eq!(error.code(), "instructions_unreadable");
        assert!(
            matches!(&error, Failure::Unreadable { path, .. } if *path == root.join("AGENTS.md"))
        );
        // An AGENTS.md whose metadata fails is reported, not skipped; the
        // global file takes the same path.
        std::fs::remove_file(root.join("AGENTS.md")).unwrap();
        std::os::unix::fs::symlink("AGENTS.md", root.join("AGENTS.md")).unwrap();
        assert_eq!(
            instructions(&root, None, &[]).unwrap_err().code(),
            "instructions_unreadable"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_skill_list_past_the_room_is_shortened_then_cut_rather_than_failing() {
        let root = temp("skill-room");
        let skills = root.join(".agents/skills");
        let add = |count: usize| {
            for i in 0..count {
                let dir = skills.join(format!("skill-{i:04}"));
                std::fs::create_dir_all(&dir).unwrap();
                let about = format!("description: {}\n", "d".repeat(150));
                std::fs::write(dir.join("SKILL.md"), format!("---\n{about}---\n# Body")).unwrap();
            }
        };
        let role = Profile {
            name: "lead".into(),
            path: None,
            model: None,
            tools: None,
            body: "r".repeat(20_000),
        };
        // Few: listed whole.
        add(3);
        let composed = instructions(&root, Some(&role), &[]).unwrap();
        assert!(composed.text.contains(&"d".repeat(150)));
        assert_eq!(composed.skills.len(), 3);
        // More than the room at 160 characters each: shorter summaries.
        add(180);
        let composed = instructions(&root, Some(&role), &[]).unwrap();
        assert!(composed.text.len() <= MAX_INSTRUCTIONS);
        assert_eq!(composed.skills.len(), 180);
        assert!(!composed.text.contains(&"d".repeat(81)));
        assert!(
            composed.text.ends_with(&"r".repeat(20_000)),
            "the role stays whole"
        );
        // Far more: the last by name left out, and the list says so.
        add(1200);
        let composed = instructions(&root, Some(&role), &[]).unwrap();
        assert!(composed.text.len() <= MAX_INSTRUCTIONS);
        let listed = composed.skills.len();
        assert!(listed < 1200 && listed > 100, "{listed}");
        assert_eq!(composed.skills[0].name, "skill-0000");
        assert!(composed.skills.iter().all(|s| s.summary.is_empty()));
        assert!(
            composed
                .text
                .contains(&format!("- {} more not listed, for room", 1200 - listed))
        );
        assert!(composed.text.ends_with(&"r".repeat(20_000)));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn skill_budget_fails_before_reading_an_entry_that_cannot_fit() {
        let root = temp("skill-budget");
        // Invalid UTF-8 would give instructions_unreadable if the file were read.
        std::fs::create_dir_all(root.join("cannot-fit")).unwrap();
        std::fs::write(root.join("cannot-fit/SKILL.md"), [0xff]).unwrap();
        assert_eq!(
            index(vec![root.clone()], Kind::Skills, 1)
                .unwrap_err()
                .code(),
            "instructions_limit"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn skill_index_bounds_large_directories_and_preserves_override_order() {
        let root = temp("skill-index");
        let local = root.join("local");
        let global = root.join("global");
        std::fs::create_dir_all(&local).unwrap();
        std::fs::create_dir_all(&global).unwrap();
        std::fs::write(local.join("same.md"), "# Local").unwrap();
        std::fs::write(global.join("same.md"), [0xff]).unwrap();
        let result = index(
            vec![local.clone(), global],
            Kind::Profiles,
            MAX_INSTRUCTIONS,
        )
        .unwrap();
        assert_eq!(result[0].summary, "Local");
        for i in 0..1000 {
            std::fs::write(local.join(format!("role-{i:04}.md")), "x".repeat(160)).unwrap();
        }
        assert_eq!(
            index(vec![local], Kind::Profiles, MAX_INSTRUCTIONS)
                .unwrap_err()
                .code(),
            "instructions_limit"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_workspace_without_files_gets_the_preamble_only() {
        let root = temp("bare");
        let composed = instructions(&root, None, &[]).unwrap();
        assert_eq!(composed.sources, Vec::new());
        assert!(composed.skills.is_empty() || composed.text.contains("# Skills"));
        assert!(composed.text.starts_with(PREAMBLE));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn memory_indexes_go_in_as_they_are_after_the_indexes_and_before_the_role() {
        let root = temp("memory");
        std::fs::write(root.join("AGENTS.md"), "rule").unwrap();
        let user = root.join("user.md");
        let index = "# Memory in /u\n\n- short-replies (feedback, verified 2026-10-10): one line";
        std::fs::write(&user, format!("{index}\n")).unwrap();
        let empty = root.join("empty.md");
        std::fs::write(&empty, "\n").unwrap();
        let missing = root.join("missing.md");
        let role = Profile::parse("lead", None, "Lead.");
        let memory = [user.clone(), empty, missing];
        let composed = instructions(&root, Some(&role), &memory).unwrap();
        assert_eq!(
            composed.memory,
            [Source {
                path: user.clone(),
                bytes: index.len()
            }]
        );
        assert!(
            composed
                .text
                .ends_with(&format!("\n\n{index}\n\n# Role: lead\n\nLead.")),
            "{}",
            composed.text
        );
        // An index that cannot be read fails the bot, as an AGENTS.md does.
        std::fs::write(&user, [0xff, 0xfe]).unwrap();
        let error = instructions(&root, None, &memory).unwrap_err();
        assert_eq!(error.code(), "instructions_unreadable");
        // So does one behind a link to a folder that is gone.
        std::os::unix::fs::symlink(root.join("gone"), root.join("link")).unwrap();
        let behind = [root.join("link/MEMORY.md")];
        let error = instructions(&root, None, &behind).unwrap_err();
        assert_eq!(error.code(), "instructions_unreadable");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_worktree_finds_its_project_and_memory_through_the_main_checkout() {
        let home = temp("memory-home");
        let repo = temp("memory-repo");
        let git = |dir: &Path, line: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(line)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{line:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&repo, &["init", "-q"]);
        std::fs::create_dir_all(repo.join("web/.agents")).unwrap();
        std::fs::write(repo.join("web/.agents/project.toml"), "name = \"old\"\n").unwrap();
        git(&repo, &["add", "web"]);
        git(&repo, &["commit", "-qm", "x"]);
        // The main checkout's file is what counts, committed or not.
        std::fs::write(repo.join("web/.agents/project.toml"), "name = \"demo\"\n").unwrap();
        let worktree = repo.with_extension("worktree");
        let _ = std::fs::remove_dir_all(&worktree);
        let at = worktree.to_str().unwrap();
        git(&repo, &["worktree", "add", "-q", "-b", "task", at, "HEAD"]);
        assert_eq!(
            project(&worktree.join("web")).unwrap().as_deref(),
            Some("demo")
        );
        assert_eq!(project(&repo.join("web")).unwrap().as_deref(), Some("demo"));
        assert_eq!(project(&repo).unwrap(), None);
        let memory = home.join(".agents/memory");
        assert_eq!(
            memory_indexes_from(&worktree.join("web"), Some(&home)).unwrap(),
            [
                memory.join("MEMORY.md"),
                memory.join("projects/demo/MEMORY.md")
            ]
        );
        assert_eq!(
            memory_indexes_from(&repo, Some(&home)).unwrap(),
            [memory.join("MEMORY.md")]
        );
        assert_eq!(
            memory_indexes_from(&repo, None).unwrap(),
            Vec::<PathBuf>::new()
        );
        // Named, a project's own, whatever folder is above it.
        assert_eq!(
            indexes_of(Some("web"), Some(&home)),
            [
                memory.join("MEMORY.md"),
                memory.join("projects/web/MEMORY.md")
            ]
        );
        assert_eq!(indexes_of(Some("web"), None), Vec::<PathBuf>::new());
        // A worktree's checkout is the main one.
        let top = repo.canonicalize().unwrap();
        assert_eq!(
            checkout_from(&worktree.join("web"), Some(&home)),
            Some(top.clone())
        );
        assert_eq!(checkout_from(&repo.join("web"), Some(&home)), Some(top));
        // A project above a repository, or the home folder's, is not its.
        let nested = repo.join("web/inner");
        std::fs::create_dir_all(nested.join(".git")).unwrap();
        assert_eq!(project(&nested).unwrap(), None);
        assert_eq!(
            checkout_from(&nested, Some(&home)),
            Some(nested.canonicalize().unwrap())
        );
        std::fs::create_dir_all(home.join(".agents")).unwrap();
        std::fs::create_dir_all(home.join("Developer/plain")).unwrap();
        std::fs::write(home.join(".agents/project.toml"), "name = \"me\"\n").unwrap();
        assert_eq!(
            project_from(&home.join("Developer/plain"), Some(&home)).unwrap(),
            None
        );
        assert_eq!(
            project_from(&home, Some(&home)).unwrap(),
            Some(("me".into(), home.canonicalize().unwrap()))
        );
        // Nor is a home folder that is a repository a folder's checkout.
        std::fs::create_dir_all(home.join(".git")).unwrap();
        assert_eq!(
            checkout_from(&home.join("Developer/plain"), Some(&home)),
            None
        );
        assert_eq!(
            checkout_from(&home, Some(&home)),
            Some(home.canonicalize().unwrap())
        );
        std::fs::remove_dir_all(home.join(".git")).unwrap();
        std::fs::write(repo.join("web/.agents/project.toml"), "name = \"../up\"\n").unwrap();
        let error = project(&repo.join("web")).unwrap_err();
        assert_eq!(error.code(), "instructions_unreadable");
        git(&repo, &["worktree", "remove", "--force", at]);
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn a_clients_own_roles_are_read_but_not_offered_as_peer_roles() {
        let root = temp("client-roles");
        std::fs::create_dir_all(root.join(".agents/agents")).unwrap();
        for name in ["lead", "reviewer"] {
            std::fs::write(
                root.join(".agents/agents").join(format!("{name}.md")),
                format!("---\ndescription: the {name}\n---\nBe the {name}.\n"),
            )
            .unwrap();
        }
        let composed = instructions(&root, None, &[]).unwrap();
        let listed: Vec<&str> = composed.profiles.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(listed, ["reviewer"]);
        assert!(!composed.text.contains("- lead:"));
        assert!(client_role("Lead"));
        // The file still replaces the client's text when it starts a bot in that role.
        let role = profile(&root, "lead").unwrap().unwrap();
        assert_eq!(role.body, "Be the lead.");
        let composed = instructions(&root, Some(&role), &[]).unwrap();
        assert!(composed.text.ends_with("# Role: lead\n\nBe the lead."));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn skills_and_profiles_read_front_matter_and_files_from_other_harnesses_load() {
        let root = temp("front");
        std::fs::create_dir_all(root.join(".agents/skills/release")).unwrap();
        std::fs::create_dir_all(root.join(".agents/agents")).unwrap();
        std::fs::write(
            root.join(".agents/skills/release/SKILL.md"),
            "---\nname: release\ndescription: \"Cut a release: tag, notes, publish\"\nlicense: MIT\n---\n# Releasing\n",
        )
        .unwrap();
        // A Claude Code agent file: keys this client does not read are ignored.
        std::fs::write(
            root.join(".agents/agents/reviewer.md"),
            "---\nname: reviewer\ndescription: Reviews a diff for bugs\nmodel: openai/gpt-6-luna\ntools:\n  - read\n  - 'shell'\ncolor: red\n---\n\nYou review changes. Report bugs only.\n",
        )
        .unwrap();
        // A file --profile cannot name is not offered as a role.
        for unusable in ["my role.md", ".draft.md", "rôle.md"] {
            std::fs::write(root.join(".agents/agents").join(unusable), "x").unwrap();
        }
        let composed = instructions(&root, None, &[]).unwrap();
        assert_eq!(composed.profiles.len(), 1);
        assert_eq!(composed.skills[0].name, "release");
        assert_eq!(
            composed.skills[0].summary,
            "Cut a release: tag, notes, publish"
        );
        assert_eq!(composed.profiles[0].name, "reviewer");
        assert!(composed.text.contains("# Profiles"));
        assert!(
            composed
                .text
                .contains("- reviewer: Reviews a diff for bugs (")
        );
        assert!(!composed.text.contains("# Role"));
        let role = profile(&root, "reviewer").unwrap().unwrap();
        assert_eq!(role.model.as_deref(), Some("openai/gpt-6-luna"));
        assert_eq!(
            role.tools,
            Some(vec!["read".to_owned(), "shell".to_owned()])
        );
        assert_eq!(role.body, "You review changes. Report bugs only.");
        let composed = instructions(&root, Some(&role), &[]).unwrap();
        assert!(
            composed
                .text
                .ends_with("\n\n# Role: reviewer\n\nYou review changes. Report bugs only.")
        );
        assert_eq!(profile(&root, "missing").unwrap(), None);
        for bad in ["", "../x", ".hidden", "a/b"] {
            assert_eq!(profile(&root, bad).unwrap_err().code(), "profile_invalid");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_profile_builds_on_another_and_carries_skills() {
        let root = temp("extends");
        let agents = root.join(".agents/agents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::create_dir_all(root.join(".agents/skills/triage")).unwrap();
        std::fs::write(
            root.join(".agents/skills/triage/SKILL.md"),
            "---\nname: triage\ndescription: Sort asks\n---\nSort every ask by owner.\n",
        )
        .unwrap();
        let builtins = [("lead", "---\nmodel: a/lead\n---\nBe the lead.")];
        // A lead variant on the client's lead: the lead's text first, its
        // model where the variant names none, then the skill whole.
        std::fs::write(
            agents.join("my-lead.md"),
            "---\nextends: lead\ndescription: A careful lead\nskills: [triage]\n---\nReview each change before it lands.\n",
        )
        .unwrap();
        let role = profile_with(&root, "my-lead", &builtins).unwrap().unwrap();
        assert_eq!(role.name, "my-lead");
        assert_eq!(role.path, Some(agents.join("my-lead.md")));
        assert_eq!(role.model.as_deref(), Some("a/lead"));
        assert_eq!(
            role.body,
            "Be the lead.\n\nReview each change before it lands.\n\n## Skill: triage\n\nSort every ask by owner."
        );
        // Without the client's lead to build on, the base is missing.
        assert_eq!(
            profile(&root, "my-lead").unwrap_err().code(),
            "profile_not_found"
        );
        // A folder's lead.md on the lead it shadows, here the client's.
        std::fs::write(
            agents.join("lead.md"),
            "---\nextends: lead\nmodel: b/mine\n---\nAlso: be brief.",
        )
        .unwrap();
        let lead = profile_with(&root, "lead", &builtins).unwrap().unwrap();
        assert_eq!(lead.body, "Be the lead.\n\nAlso: be brief.");
        assert_eq!(lead.model.as_deref(), Some("b/mine"));
        // The variant now builds on that one, which builds on the client's.
        let role = profile_with(&root, "my-lead", &builtins).unwrap().unwrap();
        assert!(
            role.body.starts_with(
                "Be the lead.\n\nAlso: be brief.\n\nReview each change before it lands."
            )
        );
        assert_eq!(role.model.as_deref(), Some("b/mine"));
        // Neither variant is offered as a role to start a peer in.
        std::fs::write(
            agents.join("reviewer.md"),
            "---\nextends: careful\nskills:\n  - triage\n  - triage\n---\nReview.",
        )
        .unwrap();
        std::fs::write(
            agents.join("careful.md"),
            "---\ntools: [read]\n---\nBe careful.",
        )
        .unwrap();
        let names: Vec<_> = instructions(&root, None, &[])
            .unwrap()
            .profiles
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, ["careful", "reviewer"]);
        // So is a variant of a variant.
        std::fs::write(
            agents.join("strict-lead.md"),
            "---\nextends: my-lead\n---\nStricter still.",
        )
        .unwrap();
        let names: Vec<_> = instructions(&root, None, &[])
            .unwrap()
            .profiles
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, ["careful", "reviewer"]);
        // Any profile builds on another; a skill named twice comes once.
        let reviewer = profile(&root, "reviewer").unwrap().unwrap();
        assert_eq!(reviewer.tools, Some(vec!["read".to_owned()]));
        assert_eq!(
            reviewer.body,
            "Be careful.\n\nReview.\n\n## Skill: triage\n\nSort every ask by owner."
        );
        // A circle, a missing skill and a bad name are each reported.
        std::fs::write(
            agents.join("careful.md"),
            "---\nextends: reviewer\n---\nBe careful.",
        )
        .unwrap();
        let circle = profile(&root, "reviewer").unwrap_err();
        assert_eq!(circle.code(), "profile_invalid");
        assert!(
            circle
                .to_string()
                .contains("reviewer -> careful -> reviewer"),
            "{circle}"
        );
        std::fs::write(
            agents.join("careful.md"),
            "---\nskills: [nothing]\n---\nBe careful.",
        )
        .unwrap();
        assert_eq!(
            profile(&root, "careful").unwrap_err().code(),
            "skill_not_found"
        );
        std::fs::write(
            agents.join("careful.md"),
            "---\nextends: ../lead\n---\nBe careful.",
        )
        .unwrap();
        assert_eq!(
            profile(&root, "careful").unwrap_err().code(),
            "profile_invalid"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn profile_folders_are_bounded_and_unreadable_files_do_not_fall_back() {
        let root = temp("entries");
        std::fs::create_dir_all(root.join(".agents/agents")).unwrap();
        for i in 0..=MAX_ENTRIES {
            std::fs::write(root.join(format!(".agents/agents/{i}.txt")), "").unwrap();
        }
        let error = index(
            search(&root, Kind::Profiles),
            Kind::Profiles,
            MAX_INSTRUCTIONS,
        )
        .unwrap_err();
        assert!(matches!(&error, Failure::TooMany { .. }));
        assert_eq!(error.code(), "instructions_limit");
        // A workspace file that errors other than by being absent is
        // reported, never replaced by the user's file of the same name. A
        // link to itself fails with ELOOP, even for root.
        std::fs::remove_dir_all(root.join(".agents/agents")).unwrap();
        std::fs::create_dir_all(root.join(".agents/agents")).unwrap();
        std::os::unix::fs::symlink("reviewer.md", root.join(".agents/agents/reviewer.md")).unwrap();
        assert_eq!(
            profile(&root, "reviewer").unwrap_err().code(),
            "instructions_unreadable"
        );
        assert_eq!(
            instructions(&root, None, &[]).unwrap_err().code(),
            "instructions_unreadable"
        );
        std::fs::remove_file(root.join(".agents/agents/reviewer.md")).unwrap();
        std::fs::create_dir_all(root.join(".agents/skills/review")).unwrap();
        std::os::unix::fs::symlink("SKILL.md", root.join(".agents/skills/review/SKILL.md"))
            .unwrap();
        assert_eq!(
            instructions(&root, None, &[]).unwrap_err().code(),
            "instructions_unreadable"
        );
        std::fs::remove_file(root.join(".agents/skills/review/SKILL.md")).unwrap();
        std::fs::write(
            root.join(".agents/agents/half.md"),
            "---\nmodel: m\nYou review.\n",
        )
        .unwrap();
        assert_eq!(
            profile(&root, "half").unwrap_err().code(),
            "instructions_unreadable"
        );
        std::fs::remove_file(root.join(".agents/agents/half.md")).unwrap();
        // A workspace that is the home folder is searched once.
        let home = home().unwrap();
        std::fs::create_dir_all(&home).unwrap();
        assert_eq!(search(&root, Kind::Profiles).len(), 2);
        assert_eq!(search(&home, Kind::Profiles).len(), 1);
        // A role too large to start is still listed under its name, so it
        // keeps shadowing the user's role of that name; starting it fails
        // explicitly instead.
        let big = std::fs::File::create(root.join(".agents/agents/huge.md")).unwrap();
        big.set_len(MAX_INSTRUCTIONS as u64 + 1).unwrap();
        assert!(
            instructions(&root, None, &[])
                .unwrap()
                .profiles
                .iter()
                .any(|p| p.name == "huge")
        );
        assert_eq!(
            profile(&root, "huge").unwrap_err().code(),
            "instructions_limit"
        );
        std::fs::remove_file(root.join(".agents/agents/huge.md")).unwrap();
        // A broken file shadowed by an override is never probed.
        let other = temp("shadowed");
        std::fs::create_dir_all(other.join(".agents/agents")).unwrap();
        std::fs::write(other.join(".agents/agents/reviewer.md"), "You review.").unwrap();
        std::fs::create_dir_all(root.join(".agents/agents")).unwrap();
        std::os::unix::fs::symlink("reviewer.md", root.join(".agents/agents/reviewer.md")).unwrap();
        let dirs = vec![other.join(".agents/agents"), root.join(".agents/agents")];
        assert_eq!(
            index(dirs, Kind::Profiles, MAX_INSTRUCTIONS).unwrap().len(),
            1
        );
        std::fs::remove_file(root.join(".agents/agents/reviewer.md")).unwrap();
        let _ = std::fs::remove_dir_all(&other);
        // A link to nothing is present, so it is reported rather than
        // letting the user's entry of the same name stand in for it.
        std::os::unix::fs::symlink("gone.md", root.join(".agents/agents/reviewer.md")).unwrap();
        assert_eq!(
            profile(&root, "reviewer").unwrap_err().code(),
            "instructions_unreadable"
        );
        std::fs::remove_file(root.join(".agents/agents/reviewer.md")).unwrap();
        std::fs::remove_dir_all(root.join(".agents/skills/review")).unwrap();
        std::os::unix::fs::symlink("gone", root.join(".agents/skills/review")).unwrap();
        assert_eq!(
            instructions(&root, None, &[]).unwrap_err().code(),
            "instructions_unreadable"
        );
        std::fs::remove_file(root.join(".agents/skills/review")).unwrap();
        std::os::unix::fs::symlink("gone.md", root.join("AGENTS.md")).unwrap();
        assert_eq!(
            instructions(&root, None, &[]).unwrap_err().code(),
            "instructions_unreadable"
        );
        std::fs::remove_file(root.join("AGENTS.md")).unwrap();
        // So is a dangling folder on the way: `.agents` itself, or the skills
        // folder, whose absence would otherwise let home's entries stand in.
        std::fs::rename(root.join(".agents"), root.join("agents.real")).unwrap();
        std::os::unix::fs::symlink("gone", root.join(".agents")).unwrap();
        assert_eq!(
            profile(&root, "reviewer").unwrap_err().code(),
            "instructions_unreadable"
        );
        std::fs::remove_file(root.join(".agents")).unwrap();
        std::fs::rename(root.join("agents.real"), root.join(".agents")).unwrap();
        std::fs::remove_dir_all(root.join(".agents/skills")).unwrap();
        std::os::unix::fs::symlink("gone", root.join(".agents/skills")).unwrap();
        assert_eq!(
            instructions(&root, None, &[]).unwrap_err().code(),
            "instructions_unreadable"
        );
        std::fs::remove_file(root.join(".agents/skills")).unwrap();
        std::fs::create_dir_all(root.join(".agents/skills")).unwrap();
        // A stray file where a skill folder would be is simply not a skill.
        std::fs::remove_dir_all(root.join(".agents/skills")).unwrap();
        std::fs::create_dir_all(root.join(".agents/skills")).unwrap();
        std::fs::write(root.join(".agents/skills/README.md"), "notes").unwrap();
        assert!(instructions(&root, None, &[]).unwrap().skills.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn front_matter_takes_flow_and_inline_lists_and_leaves_plain_files_whole() {
        let (front, body) = front_matter("---\ntools: [read, edit]\n---\nbody\n");
        assert_eq!(
            front.tools,
            Some(vec!["read".to_owned(), "edit".to_owned()])
        );
        assert_eq!(body, "body\n");
        let (front, _) = front_matter("---\ntools: read, shell\nmodel: ''\n---\n");
        assert_eq!(
            front.tools,
            Some(vec!["read".to_owned(), "shell".to_owned()])
        );
        assert_eq!(
            Profile::parse("x", None, "---\nmodel: ''\n---\nhi").model,
            None
        );
        // Comments and block scalars are YAML, not part of the value.
        let (front, _) = front_matter(
            "---\ntools: [shell, wait] # defaults\nmodel: openai/foo # preferred\ndescription: >-\n  Reviews code,\n  #1 on\ntitle: 'a # b'\n---\n",
        );
        assert_eq!(
            front.tools,
            Some(vec!["shell".to_owned(), "wait".to_owned()])
        );
        assert_eq!(front.model.as_deref(), Some("openai/foo"));
        assert_eq!(front.description.as_deref(), Some("Reviews code, #1 on"));
        let (front, _) =
            front_matter("---\ntools:\n  - read # first\n\n  # the rest\n  - 'edit'\n---\n");
        assert_eq!(
            front.tools,
            Some(vec!["read".to_owned(), "edit".to_owned()])
        );
        let (front, body) = front_matter("# Title\n\ntext");
        assert_eq!(front, Front::default());
        assert_eq!(body, "# Title\n\ntext");
    }
}
