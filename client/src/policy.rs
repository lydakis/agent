//! What a human-facing client tells a new bot. The daemon stores whatever
//! text it is given and never composes any; this is where clients agree on
//! the composition, so a bot created from the app or the CLI with `--agents`
//! reads the same way.
//!
//! Layers, in this order: the harness preamble (how to delegate and collect
//! results through this runtime), every AGENTS.md and `.agents/AGENTS.md`
//! from the workspace up to the filesystem root plus the user's global one,
//! an index of skills the bot can open with its `read` tool and of profiles
//! it can start peers in, and the bot's own role when it is started in one.
//! The text is a stable prefix on purpose: it rides the provider's prompt
//! cache after the first turn, so it changes only when a file changes.
use std::path::{Path, PathBuf};

/// The daemon refuses instructions above 64 KiB; stay under it with room
/// for the prompt cache to matter.
pub const MAX_INSTRUCTIONS: usize = 60 * 1024;

pub const PREAMBLE: &str = "To delegate a subtask to another agent with its own conversation, run \
\"$AGENT_BIN\" run --detach --new --bot NAME -- TASK from the shell; it prints a turn handle immediately. \
Add --model PROVIDER/MODEL to give it one of the models \"$AGENT_BIN\" models lists; without it, it gets yours. \
Continue an existing agent with \"$AGENT_BIN\" run --detach --bot NAME -- TASK. \
Collect results with the wait tool on that handle; it returns the peer's status and final text. \
Blocking run/follow inside a shell tool is rejected. \
Use \"$AGENT_BIN\" fork --source NAME --bot NEW to branch an agent from where it is now, mid-turn included; --checkpoint N branches from an earlier point in its history. \
$AGENT_BOT and $AGENT_BOT_ID identify you; $AGENT_PARENT and $AGENT_PARENT_ID, when set, identify the agent that created you. \
To contact that creator, run \
\"$AGENT_BIN\" run --detach --bot \"$AGENT_PARENT\" --bot-id \"$AGENT_PARENT_ID\" -- TASK; \
the identity check refuses a deleted creator or a replacement with the same name. \
Your final reply already reaches whoever waits on your turn, so contact your creator only to ask something you need, \
not to report results.";

/// What human-facing clients tell a new bot's summarizer at compaction. The daemon has
/// no such text; a bot created without any never compacts.
pub const DEFAULT_COMPACTION_INSTRUCTIONS: &str = "You are summarizing the earlier part of an agent's conversation so the agent can continue \
with the summary in place of those turns. Any earlier summary is given first; merge it with the new turns, do not restart. \
Write, in order: the goal; every rule, constraint, or preference the user stated, verbatim where wording matters; \
what is done, in progress, and blocked; key decisions and why; files read or changed; open questions; next steps. \
Keep exact names, paths, commands, values, and error text. Omit chatter, repeated tool output, and anything superseded. \
Reply with the summary only.";

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
}
impl Failure {
    /// A stable code for programs, in the daemon's error style.
    pub fn code(&self) -> &'static str {
        match self {
            Failure::TooLong { .. } | Failure::TooMany { .. } => "instructions_limit",
            Failure::Unreadable { .. } => "instructions_unreadable",
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
        }
    }
}

fn home() -> Option<PathBuf> {
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
/// spaces; `tools` is a comma-separated line, a `[a, b]` flow list, or `- a`
/// items below it. Comments are dropped.
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
    let mut in_tools = false;
    let mut lines = block.lines().peekable();
    while let Some(line) = lines.next() {
        if uncomment(line).trim().is_empty() {
            continue;
        }
        if in_tools && let Some(item) = line.trim_start().strip_prefix("- ") {
            front
                .tools
                .get_or_insert_with(Vec::new)
                .push(unquote(uncomment(item)));
            continue;
        }
        in_tools = false;
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
        match key.trim() {
            "name" => front.name = Some(scalar(&mut lines)),
            "description" => front.description = Some(scalar(&mut lines)),
            "model" => front.model = Some(scalar(&mut lines)),
            "tools" if value.is_empty() => {
                in_tools = true;
                front.tools = Some(Vec::new());
            }
            "tools" => {
                let list = value.trim_start_matches('[').trim_end_matches(']');
                front.tools = Some(
                    list.split(',')
                        .map(unquote)
                        .filter(|t| !t.is_empty())
                        .collect(),
                );
            }
            _ => {}
        }
    }
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

/// Roles a client starts its own bots in: the app's coordinator and its
/// swarms' members. A `.agents/agents` file of one of these names replaces
/// the client's text for that role, so it is not a role to start a peer in,
/// and the Profiles index leaves it out.
pub const CLIENT_ROLES: [&str; 3] = ["coordinator", "swarm-flat", "swarm-council"];

/// Whether a profile file's stem is a client role. Case is ignored: on a
/// case-insensitive filesystem (macOS's default) `Coordinator.md` is the
/// file `profile(_, "coordinator")` reads.
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
    fn header(self) -> &'static str {
        match self {
            Kind::Skills => {
                "\n\n# Skills\n\nRead a skill file with the read tool when its subject comes up.\n"
            }
            Kind::Profiles => {
                "\n\n# Profiles\n\nStart an agent in one of these roles with \"$AGENT_BIN\" run --detach --new --profile ROLE --bot NAME -- TASK.\n"
            }
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

fn entry_row(entry: &Entry) -> String {
    format!(
        "\n- {}: {} ({})",
        entry.name,
        entry.summary,
        entry.path.display()
    )
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
            if found.contains_key(&name) {
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
    Ok(found.into_values().collect())
}

/// A role a bot is started in: the body of `.agents/agents/<name>.md`, and
/// the model and tools its front matter names, if any.
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
fn profile_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// The profile named `name` for a bot in `workspace`: the workspace's own
/// file, else the user's; `None` when neither exists.
pub fn profile(workspace: &Path, name: &str) -> Result<Option<Profile>, Failure> {
    if !profile_name(name) {
        return Err(Failure::Unreadable {
            path: PathBuf::from(name),
            reason: "a profile name is letters, digits, '-', '_' and '.'".into(),
        });
    }
    for dir in search(workspace, Kind::Profiles) {
        let path = dir.join(format!("{name}.md"));
        if !is_file(&path)? {
            continue;
        }
        let Some(text) = read_bounded(&path, MAX_INSTRUCTIONS)? else {
            return Err(Failure::TooLong {
                total: std::fs::metadata(&path).map_or(MAX_INSTRUCTIONS + 1, |m| m.len() as usize),
                path,
            });
        };
        // An unclosed head would read the whole role as front matter and
        // start the bot without it.
        if (text.starts_with("---\n") || text.starts_with("---\r\n"))
            && !text[3..].contains("\n---")
        {
            return Err(Failure::Unreadable {
                path,
                reason: "front matter opened with --- is not closed".into(),
            });
        }
        return Ok(Some(Profile::parse(name, Some(path), &text)));
    }
    Ok(None)
}

/// The full text for a new bot in `workspace`, in `role` when given. Fails
/// rather than truncates when the files do not fit: a silently shortened
/// AGENTS.md is worse than none.
pub fn instructions(workspace: &Path, role: Option<&Profile>) -> Result<Instructions, Failure> {
    let mut text = String::from(PREAMBLE);
    let mut sources = Vec::new();
    for path in agents_files(workspace)? {
        // Read no more than what could still fit; a file past the budget
        // fails on its size, not after being copied into memory.
        let header = format!("\n\n# Instructions from {}\n\n", path.display());
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
            continue;
        }
        let block = format!("{header}{body}");
        if text.len() + block.len() > MAX_INSTRUCTIONS {
            return Err(Failure::TooLong {
                path,
                total: text.len() + block.len(),
            });
        }
        text.push_str(&block);
        sources.push(Source {
            path,
            bytes: body.len(),
        });
    }
    let mut lists = Vec::new();
    for kind in [Kind::Skills, Kind::Profiles] {
        let entries = index(
            search(workspace, kind),
            kind,
            MAX_INSTRUCTIONS.saturating_sub(text.len() + kind.header().len()),
        )?;
        if !entries.is_empty() {
            let mut block = String::from(kind.header());
            for entry in &entries {
                block.push_str(&entry_row(entry));
            }
            if text.len() + block.len() > MAX_INSTRUCTIONS {
                return Err(Failure::TooLong {
                    path: entries[0].path.clone(),
                    total: text.len() + block.len(),
                });
            }
            text.push_str(&block);
        }
        lists.push(entries);
    }
    // The role comes last: the most specific text a bot is given.
    if let Some(role) = role.filter(|r| !r.body.is_empty()) {
        let block = format!("\n\n# Role: {}\n\n{}", role.name, role.body);
        if text.len() + block.len() > MAX_INSTRUCTIONS {
            return Err(Failure::TooLong {
                path: role
                    .path
                    .clone()
                    .unwrap_or_else(|| PathBuf::from(&role.name)),
                total: text.len() + block.len(),
            });
        }
        text.push_str(&block);
    }
    let profiles = lists.pop().unwrap_or_default();
    let skills = lists.pop().unwrap_or_default();
    Ok(Instructions {
        text,
        sources,
        skills,
        profiles,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let composed = instructions(&deep, None).unwrap();
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
        let composed = instructions(&deep, None).unwrap();
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
        let error = instructions(&root, None).unwrap_err();
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
        let error = instructions(&root, None).unwrap_err();
        assert!(
            matches!(&error, Failure::TooLong { path, total } if *path == root.join("AGENTS.md") && *total > 512 * 1024 * 1024)
        );
        std::fs::create_dir_all(root.join(".agents/skills/big")).unwrap();
        std::fs::write(root.join("AGENTS.md"), "rule").unwrap();
        let mut long = String::from("# Big skill\n\n");
        long.push_str(&"x".repeat(SKILL_HEAD * 4));
        std::fs::write(root.join(".agents/skills/big/SKILL.md"), long).unwrap();
        let composed = instructions(&root, None).unwrap();
        assert_eq!(composed.skills[0].summary, "Big skill");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unreadable_file_is_an_error_not_a_silent_omission() {
        let root = temp("unreadable");
        std::fs::write(root.join("AGENTS.md"), [0xff, 0xfe, b'x']).unwrap();
        let error = instructions(&root, None).unwrap_err();
        assert_eq!(error.code(), "instructions_unreadable");
        assert!(
            matches!(&error, Failure::Unreadable { path, .. } if *path == root.join("AGENTS.md"))
        );
        // An AGENTS.md whose metadata fails is reported, not skipped; the
        // global file takes the same path.
        std::fs::remove_file(root.join("AGENTS.md")).unwrap();
        std::os::unix::fs::symlink("AGENTS.md", root.join("AGENTS.md")).unwrap();
        assert_eq!(
            instructions(&root, None).unwrap_err().code(),
            "instructions_unreadable"
        );
        let _ = std::fs::remove_dir_all(&root);
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
        let composed = instructions(&root, None).unwrap();
        assert_eq!(composed.sources, Vec::new());
        assert!(composed.skills.is_empty() || composed.text.contains("# Skills"));
        assert!(composed.text.starts_with(PREAMBLE));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_clients_own_roles_are_read_but_not_offered_as_peer_roles() {
        let root = temp("client-roles");
        std::fs::create_dir_all(root.join(".agents/agents")).unwrap();
        for name in ["coordinator", "swarm-flat", "Swarm-Council", "reviewer"] {
            std::fs::write(
                root.join(".agents/agents").join(format!("{name}.md")),
                format!("---\ndescription: the {name}\n---\nBe the {name}.\n"),
            )
            .unwrap();
        }
        let composed = instructions(&root, None).unwrap();
        let listed: Vec<&str> = composed.profiles.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(listed, ["reviewer"]);
        assert!(!composed.text.contains("- coordinator:"));
        // The file still replaces the client's text when it starts a bot in that role.
        let role = profile(&root, "coordinator").unwrap().unwrap();
        assert_eq!(role.body, "Be the coordinator.");
        let composed = instructions(&root, Some(&role)).unwrap();
        assert!(
            composed
                .text
                .ends_with("# Role: coordinator\n\nBe the coordinator.")
        );
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
        let composed = instructions(&root, None).unwrap();
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
        let composed = instructions(&root, Some(&role)).unwrap();
        assert!(
            composed
                .text
                .ends_with("\n\n# Role: reviewer\n\nYou review changes. Report bugs only.")
        );
        assert_eq!(profile(&root, "missing").unwrap(), None);
        for bad in ["", "../x", ".hidden", "a/b"] {
            assert_eq!(
                profile(&root, bad).unwrap_err().code(),
                "instructions_unreadable"
            );
        }
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
            instructions(&root, None).unwrap_err().code(),
            "instructions_unreadable"
        );
        std::fs::remove_file(root.join(".agents/agents/reviewer.md")).unwrap();
        std::fs::create_dir_all(root.join(".agents/skills/review")).unwrap();
        std::os::unix::fs::symlink("SKILL.md", root.join(".agents/skills/review/SKILL.md"))
            .unwrap();
        assert_eq!(
            instructions(&root, None).unwrap_err().code(),
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
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            assert_eq!(search(&root, Kind::Profiles).len(), 2);
            assert_eq!(search(&home, Kind::Profiles).len(), 1);
        }
        // A role too large to start is still listed under its name, so it
        // keeps shadowing the user's role of that name; starting it fails
        // explicitly instead.
        let big = std::fs::File::create(root.join(".agents/agents/huge.md")).unwrap();
        big.set_len(MAX_INSTRUCTIONS as u64 + 1).unwrap();
        assert!(
            instructions(&root, None)
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
            instructions(&root, None).unwrap_err().code(),
            "instructions_unreadable"
        );
        std::fs::remove_file(root.join(".agents/skills/review")).unwrap();
        std::os::unix::fs::symlink("gone.md", root.join("AGENTS.md")).unwrap();
        assert_eq!(
            instructions(&root, None).unwrap_err().code(),
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
            instructions(&root, None).unwrap_err().code(),
            "instructions_unreadable"
        );
        std::fs::remove_file(root.join(".agents/skills")).unwrap();
        std::fs::create_dir_all(root.join(".agents/skills")).unwrap();
        // A stray file where a skill folder would be is simply not a skill.
        std::fs::remove_dir_all(root.join(".agents/skills")).unwrap();
        std::fs::create_dir_all(root.join(".agents/skills")).unwrap();
        std::fs::write(root.join(".agents/skills/README.md"), "notes").unwrap();
        assert!(instructions(&root, None).unwrap().skills.is_empty());
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
