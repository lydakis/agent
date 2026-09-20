//! What a human-facing client tells a new bot. The daemon stores whatever
//! text it is given and never composes any; this is where clients agree on
//! the composition, so a bot created from the app or the CLI with `--agents`
//! reads the same way.
//!
//! Three layers, in this order: the harness preamble (how to delegate and
//! collect results through this runtime), every AGENTS.md from the
//! workspace up to the filesystem root plus the user's global one, and an
//! index of skill files the bot can open with its `read` tool. The text is
//! a stable prefix on purpose: it rides the provider's prompt cache after
//! the first turn, so it changes only when a file changes.
use std::path::{Path, PathBuf};

/// The daemon refuses instructions above 64 KiB; stay under it with room
/// for the prompt cache to matter.
pub const MAX_INSTRUCTIONS: usize = 60 * 1024;

pub const PREAMBLE: &str = "You are a software engineering agent working in the current workspace. \
Complete the requested task using the available tools, verify your work, and finish with a short summary. \
To delegate a subtask to another agent with its own conversation, run \
\"$AGENT_BIN\" run --detach --new --bot NAME -- TASK from the shell; it prints a turn handle immediately. \
Continue an existing agent with \"$AGENT_BIN\" run --detach --bot NAME -- TASK. \
Collect results with the wait tool on that handle; it returns the peer's status and final text. \
Long commands can run with shell background=true and be collected the same way. \
Blocking run/follow inside a shell tool is rejected. \
Use \"$AGENT_BIN\" fork --source NAME --checkpoint N --bot NEW to branch an agent from an earlier point in its history. \
$AGENT_BOT is your name; $AGENT_PARENT and $AGENT_PARENT_ID, when set, identify the agent that created you. \
To contact that creator, run \
\"$AGENT_BIN\" run --detach --bot \"$AGENT_PARENT\" --bot-id \"$AGENT_PARENT_ID\" -- TASK; \
the identity check refuses a deleted creator or a replacement with the same name.";

/// One instruction file that went into the text, for the client to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub path: PathBuf,
    pub bytes: usize,
}

/// A skill: a markdown file the bot may read when the task calls for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub path: PathBuf,
    /// The file's first non-empty line, stripped of heading marks.
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instructions {
    pub text: String,
    pub sources: Vec<Source>,
    pub skills: Vec<Skill>,
}

/// Why the text could not be composed. Both are reported, never worked
/// around: a bot created without a rule it should have had is worse than
/// no bot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    TooLong { path: PathBuf, total: usize },
    Unreadable { path: PathBuf, reason: String },
}
impl Failure {
    /// A stable code for programs, in the daemon's error style.
    pub fn code(&self) -> &'static str {
        match self {
            Failure::TooLong { .. } => "instructions_limit",
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

/// The first line of a skill: at most this much is read to find it.
const SKILL_HEAD: usize = 4096;

/// AGENTS.md files that apply to `workspace`: the global one first, then
/// from the filesystem root down to the workspace, so the nearest file is
/// read last and wins where they disagree.
pub fn agents_files(workspace: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Some(global) = home().map(|h| h.join(".agent").join("AGENTS.md"))
        && global.is_file()
    {
        files.push(global);
    }
    let start = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let mut chain: Vec<PathBuf> = start
        .ancestors()
        .map(|dir| dir.join("AGENTS.md"))
        .filter(|file| file.is_file())
        .collect();
    chain.reverse();
    for file in chain {
        if !files.contains(&file) {
            files.push(file);
        }
    }
    files
}

/// Skill files: `<workspace>/.agent/skills/*.md` after `~/.agent/skills/*.md`,
/// by name, the workspace's winning on a clash.
const SKILLS_HEADER: &str =
    "\n\n# Skills\n\nRead a skill file with the read tool when its subject comes up.\n";

fn skill_row(skill: &Skill) -> String {
    format!(
        "\n- {}: {} ({})",
        skill.name,
        skill.summary,
        skill.path.display()
    )
}

fn skills(workspace: &Path, budget: usize) -> Result<Vec<Skill>, Failure> {
    // Visit the winning directory first, so a later directory can only add
    // entries. A budget failure can never be undone by an override.
    let mut dirs = vec![workspace.join(".agent/skills")];
    if let Some(h) = home() {
        dirs.push(h.join(".agent/skills"));
    }
    skills_from(dirs, budget)
}

fn skills_from(dirs: Vec<PathBuf>, budget: usize) -> Result<Vec<Skill>, Failure> {
    let mut found = std::collections::BTreeMap::<String, Skill>::new();
    let mut used = 0usize;
    for dir in dirs {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
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
            let path = entry
                .map_err(|error| Failure::Unreadable {
                    path: dir.clone(),
                    reason: error.to_string(),
                })?
                .path();
            if !path.extension().is_some_and(|x| x == "md") || !path.is_file() {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else {
                continue;
            };
            if found.contains_key(&name) {
                continue;
            }
            let mut skill = Skill {
                name,
                path,
                summary: String::new(),
            };
            let too_long = |size| Failure::TooLong {
                path: skill.path.clone(),
                total: MAX_INSTRUCTIONS.saturating_sub(budget) + size,
            };
            let minimum = used + skill_row(&skill).len();
            if minimum > budget {
                return Err(too_long(minimum));
            }
            let mut bytes = read_head(&skill.path, SKILL_HEAD)?;
            bytes.truncate(SKILL_HEAD);
            let text = match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(error) if error.utf8_error().error_len().is_none() => {
                    let valid = error.utf8_error().valid_up_to();
                    String::from_utf8_lossy(&error.into_bytes()[..valid]).into_owned()
                }
                Err(error) => {
                    return Err(Failure::Unreadable {
                        path: skill.path,
                        reason: error.utf8_error().to_string(),
                    });
                }
            };
            skill.summary = text
                .lines()
                .map(|l| l.trim().trim_start_matches('#').trim())
                .find(|l| !l.is_empty())
                .map(|l| l.chars().take(160).collect())
                .unwrap_or_default();
            used += skill_row(&skill).len();
            if used > budget {
                return Err(too_long(used));
            }
            found.insert(skill.name.clone(), skill);
        }
    }
    Ok(found.into_values().collect())
}

/// The full text for a new bot in `workspace`. Fails rather than truncates
/// when the files do not fit: a silently shortened AGENTS.md is worse than
/// none.
pub fn instructions(workspace: &Path) -> Result<Instructions, Failure> {
    let mut text = String::from(PREAMBLE);
    let mut sources = Vec::new();
    for path in agents_files(workspace) {
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
    let skills = skills(
        workspace,
        MAX_INSTRUCTIONS.saturating_sub(text.len() + SKILLS_HEADER.len()),
    )?;
    if !skills.is_empty() {
        let mut block = String::from(SKILLS_HEADER);
        for skill in &skills {
            block.push_str(&skill_row(skill));
        }
        if text.len() + block.len() > MAX_INSTRUCTIONS {
            return Err(Failure::TooLong {
                path: skills[0].path.clone(),
                total: text.len() + block.len(),
            });
        }
        text.push_str(&block);
    }
    Ok(Instructions {
        text,
        sources,
        skills,
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
        std::fs::create_dir_all(deep.join(".agent").join("skills")).unwrap();
        std::fs::write(root.join("AGENTS.md"), "outer rule").unwrap();
        std::fs::write(deep.join("AGENTS.md"), "# inner\n\ninner rule").unwrap();
        std::fs::write(
            deep.join(".agent/skills/deploy.md"),
            "# Deploy\n\nShip a release safely.",
        )
        .unwrap();
        std::fs::write(deep.join(".agent/skills/notes.txt"), "not a skill").unwrap();
        let files = agents_files(&deep);
        assert_eq!(
            files.iter().rev().take(2).collect::<Vec<_>>(),
            vec![&deep.join("AGENTS.md"), &root.join("AGENTS.md")]
        );
        let composed = instructions(&deep).unwrap();
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
    fn oversized_files_fail_instead_of_being_cut() {
        let root = temp("big");
        std::fs::write(root.join("AGENTS.md"), "x".repeat(MAX_INSTRUCTIONS)).unwrap();
        let error = instructions(&root).unwrap_err();
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
        let error = instructions(&root).unwrap_err();
        assert!(
            matches!(&error, Failure::TooLong { path, total } if *path == root.join("AGENTS.md") && *total > 512 * 1024 * 1024)
        );
        std::fs::create_dir_all(root.join(".agent/skills")).unwrap();
        std::fs::write(root.join("AGENTS.md"), "rule").unwrap();
        let mut long = String::from("# Big skill\n\n");
        long.push_str(&"x".repeat(SKILL_HEAD * 4));
        std::fs::write(root.join(".agent/skills/big.md"), long).unwrap();
        let composed = instructions(&root).unwrap();
        assert_eq!(composed.skills[0].summary, "Big skill");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unreadable_file_is_an_error_not_a_silent_omission() {
        let root = temp("unreadable");
        std::fs::write(root.join("AGENTS.md"), [0xff, 0xfe, b'x']).unwrap();
        let error = instructions(&root).unwrap_err();
        assert_eq!(error.code(), "instructions_unreadable");
        assert!(
            matches!(&error, Failure::Unreadable { path, .. } if *path == root.join("AGENTS.md"))
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn skill_budget_fails_before_reading_an_entry_that_cannot_fit() {
        let root = temp("skill-budget");
        // Invalid UTF-8 would give instructions_unreadable if the file were read.
        std::fs::write(root.join("cannot-fit.md"), [0xff]).unwrap();
        assert_eq!(
            skills_from(vec![root.clone()], 1).unwrap_err().code(),
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
        let result = skills_from(vec![local.clone(), global], MAX_INSTRUCTIONS).unwrap();
        assert_eq!(result[0].summary, "Local");
        for i in 0..1000 {
            std::fs::write(local.join(format!("skill-{i:04}.md")), "x".repeat(160)).unwrap();
        }
        assert_eq!(
            skills_from(vec![local], MAX_INSTRUCTIONS)
                .unwrap_err()
                .code(),
            "instructions_limit"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_workspace_without_files_gets_the_preamble_only() {
        let root = temp("bare");
        let composed = instructions(&root).unwrap();
        assert_eq!(composed.sources, Vec::new());
        assert!(composed.skills.is_empty() || composed.text.contains("# Skills"));
        assert!(composed.text.starts_with(PREAMBLE));
        let _ = std::fs::remove_dir_all(&root);
    }
}
