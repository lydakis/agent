//! One-off work an upgrade brings, each retired two releases after it
//! ships. What needs judgment, or reaches what is the person's own (what
//! they wrote in a setting's value or a command, skills they changed,
//! hosts this machine knows), is a step: words in `sonny/upgrades/ID.md`
//! that the upgrade note hands the folder's lead, as release notes hand an
//! operator, with a check in code that says when it is done. What a script
//! does the same everywhere (a rewrite of Sonny's own files, or a key
//! renamed in the person's config, kept only when the file reads as the
//! same settings after) is a migration: code that runs once, through
//! [`once`]. The store's schema is neither; its migrations stay in the
//! daemon for good, since each new store replays them.
//!
//! Neither stays: each names the release it retires at, and a test fails
//! once Sonny's version reaches it, as Chromium's flags fail a test past
//! their expiry milestone. A person who skips more releases than that gets
//! what a new install gets.
use std::path::Path;

/// Something an upgrade asks the folder's lead to do for the person.
pub struct Step {
    /// Its name, as `sonny/upgrades/ID.md` and `~/.agent/sonny/tried` have it.
    pub id: &'static str,
    /// Its file: a first line `Feature: ROW`, naming the row of the feature
    /// register it belongs to by the row's first words, then what to do,
    /// for the agent.
    pub text: &'static str,
    /// Whether it is done, or there is nothing to do: until then it rides
    /// each upgrade's note, once for each Sonny.
    pub done: fn() -> bool,
    /// The release it is deleted in, two after the one it ships in: a test
    /// fails from then on.
    #[cfg_attr(not(test), allow(dead_code))]
    pub retire: &'static str,
}

impl Step {
    /// What it asks, without its first line.
    pub fn body(&self) -> &'static str {
        self.text
            .split_once('\n')
            .map_or("", |(_, rest)| rest)
            .trim()
    }

    /// The register row it belongs to, by the row's first words.
    #[cfg(test)]
    pub fn feature(&self) -> Option<&'static str> {
        let first = self.text.lines().next()?;
        Some(first.strip_prefix("Feature: ")?.trim())
    }
}

/// What this Sonny's upgrade asks agents to do. A new one is a file in
/// `sonny/upgrades` and a row here, named in its feature's history.
pub const STEPS: &[Step] = &[
    // A reviewer model set as `AGENT_JUDGE` in ~/.agent/env, unread since
    // 0.8.0, moves to `[reviewer] model` (0.9.0).
    Step {
        id: "judge-env",
        text: include_str!("../upgrades/judge-env.md"),
        done: judge_env_done,
        retire: "0.11.0",
    },
    // An automation's `if` an agent added runs in its sandbox; one added
    // before, whose writer was not recorded, does not run until added again (0.9.0).
    Step {
        id: "agent-gates",
        text: include_str!("../upgrades/agent-gates.md"),
        done: crate::trigger::no_gate_unowned,
        retire: "0.11.0",
    },
    // Fork-from-turn's snapshot refs, in the repositories of the person's
    // tasks, go with their OK (0.9.0).
    Step {
        id: "snapshot-refs",
        text: include_str!("../upgrades/snapshot-refs.md"),
        done: crate::git::no_snapshot_refs,
        retire: "0.11.0",
    },
    // A message's handle is `BOT/N` since 0.9.3; facts and skills agents
    // wrote before still name `turn:BOT/N`, the same number (0.9.4).
    Step {
        id: "message-handles",
        text: include_str!("../upgrades/message-handles.md"),
        done: message_handles_done,
        retire: "0.11.0",
    },
];

/// Whether no fact in `~/.agents/memory` and no skill in `~/.agents/skills`
/// still names a message in the form before 0.9.3, `turn:BOT/N`.
fn message_handles_done() -> bool {
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
        return true;
    };
    ["memory", "skills"]
        .iter()
        .all(|folder| !old_handle_under(&home.join(".agents").join(folder), 0))
}

/// Whether a Markdown file under `dir`, a few folders deep at most, names
/// a `turn:BOT/N` handle.
fn old_handle_under(dir: &Path, depth: usize) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => depth < 4 && old_handle_under(&path, depth + 1),
            Ok(kind) if kind.is_file() => {
                path.extension().is_some_and(|ext| ext == "md")
                    && std::fs::read_to_string(&path).is_ok_and(|text| names_old_handle(&text))
            }
            _ => false,
        }
    })
}

/// Whether `text` holds `turn:BOT/N`: `turn:`, a name with no space or
/// slash, a slash, and a digit.
fn names_old_handle(text: &str) -> bool {
    text.match_indices("turn:").any(|(at, prefix)| {
        let rest = &text[at + prefix.len()..];
        let name = rest
            .find(|c: char| c == '/' || c.is_whitespace())
            .filter(|end| *end > 0 && rest[*end..].starts_with('/'));
        name.is_some_and(|end| rest[end + 1..].starts_with(|c: char| c.is_ascii_digit()))
    })
}

/// Whether `~/.agent/env` sets no `AGENT_JUDGE`, the reviewer's model
/// before 0.8.0, which is `[reviewer] model` now.
fn judge_env_done() -> bool {
    agent_client::env_file::path().is_none_or(|file| no_judge_in(&file))
}

/// Whether the env file at `file` sets no `AGENT_JUDGE`.
fn no_judge_in(file: &Path) -> bool {
    agent_client::env_file::read(file).map_or(true, |pairs| {
        pairs.iter().all(|(key, _)| key != "AGENT_JUDGE")
    })
}

/// Something Sonny does once itself, by [`once`], for as long as it is
/// listed.
pub struct Migration {
    pub id: &'static str,
    /// The release it is deleted in, two after the one it ships in: a test
    /// fails from then on.
    #[cfg_attr(not(test), allow(dead_code))]
    pub retire: &'static str,
}

/// Every migration, through [`once`] or where it is read, so none outlives
/// its release.
pub const MIGRATIONS: &[Migration] = &[
    // Each trigger's own LaunchAgent or unit, and the backup's and
    // start-at-login's, become Sonny's scheduler's (0.8.0).
    Migration {
        id: "scheduler-job",
        retire: "0.10.0",
    },
    // Agents that posted in the commons are subscribed to its threads
    // (0.8.0).
    Migration {
        id: crate::commons::POSTERS,
        retire: "0.10.0",
    },
    // `~/.agent/triggers` becomes `~/.agent/automations`, and a restore's
    // `restored-triggers.json` `restored-automations.json`, through
    // [`once`] wherever the folder is found (`trigger::Places::moved`),
    // since the script and the scheduler read it without Sonny's start
    // (0.8.0).
    Migration {
        id: crate::trigger::MOVED,
        retire: "0.10.0",
    },
    // Sonny's old `~/.agent/trigger` script goes, once
    // `~/.agent/automation` is written (0.8.0).
    Migration {
        id: crate::skills::SCRIPT,
        retire: "0.10.0",
    },
    // `[sonny] unsandboxed` becomes allow rules in ~/.agent/rules (0.8.0).
    Migration {
        id: crate::approvals::UNSANDBOXED_RULES,
        retire: "0.10.0",
    },
    // A source's `urgent` becomes `steer`, and `[sonny] commons_digest`
    // the commons source's `digest` (0.8.0).
    Migration {
        id: crate::lookout::subs::STEER,
        retire: "0.10.0",
    },
    // Sonny's scheduler job moves to a label that names no one (0.9.0).
    Migration {
        id: crate::scheduler::RELABELED,
        retire: "0.11.0",
    },
    // Each daemon's haiku claim is kept apart from the haiku shown, in
    // `haiku.json` (0.8.1).
    Migration {
        id: crate::haiku::CLAIMS,
        retire: "0.10.0",
    },
    // Each daemon's past haikus move from `haiku.json` to `haikus.jsonl`
    // (0.9.0).
    Migration {
        id: crate::haiku::PAST,
        retire: "0.11.0",
    },
    // `[sonny] sky_haikus`, a count, becomes the switch it is and the
    // day's most, `sky_haiku_daily` (0.9.0).
    Migration {
        id: crate::haiku::DAILY,
        retire: "0.11.0",
    },
    // `[sonny] task_depth` becomes the daemon's `[daemon] depth` (0.9.0).
    Migration {
        id: crate::config::DEPTH,
        retire: "0.11.0",
    },
    // Goals are gone: the lookout's goals folder goes (0.9.0).
    Migration {
        id: crate::lookout::subs::GOALS_GONE,
        retire: "0.11.0",
    },
    // Each store's snapshot indexes, unused since fork-from-turn went,
    // go (0.9.0).
    Migration {
        id: crate::git::NO_SNAPSHOTS,
        retire: "0.11.0",
    },
    // `[judge]` in ~/.agent/config.toml becomes `[reviewer]` (0.9.1).
    Migration {
        id: crate::config::REVIEWER,
        retire: "0.11.0",
    },
    // The reviewer's companion `approver-auto` becomes `sonny-reviewer`,
    // and its base bot `approver.TAG` Sonny's own `.reviewer.TAG` (0.9.1).
    Migration {
        id: crate::reviewer::RENAMED,
        retire: "0.11.0",
    },
    // The commons' wake marks go with the wakes: an invite is the one
    // turn the commons starts (0.9.1).
    Migration {
        id: crate::commons::WOKEN_GONE,
        retire: "0.11.0",
    },
    // Quiet mode is gone: `[sonny] hours` and `quiet_after` leave the
    // config, which would refuse them, and its files and skill go (0.9.1).
    Migration {
        id: crate::config::QUIET_GONE,
        retire: "0.11.0",
    },
    // The reply check and the done check are gone: `[sonny] needs_check`
    // and `task_done_check`, which `/config` wrote, leave the config, which
    // would refuse them (0.9.1).
    Migration {
        id: crate::config::CHECKS_GONE,
        retire: "0.11.0",
    },
    // Sonny's own nightly memory cleanup, scheduled before each run started
    // an agent of its own, is scheduled again fresh, keeping its model and
    // time (0.9.0).
    Migration {
        id: crate::trigger::CLEANUP_FRESH,
        retire: "0.11.0",
    },
    // The repositories inside an agent's folder leave its writable folders:
    // the folder is the agent's whole, as Codex's is (0.9.2).
    Migration {
        id: crate::approvals::NESTED_GONE,
        retire: "0.11.0",
    },
];

/// `run` once for each `record`: a file of the ids done, a line each,
/// beside the state they changed (`~/.agent/sonny/migrated`, or one
/// beside a store for what is the store's). What fails, `run` answering
/// false, runs again next time. Whether `id` is done now.
pub fn once(record: &Path, id: &str, run: impl FnOnce() -> bool) -> bool {
    debug_assert!(
        MIGRATIONS.iter().any(|m| m.id == id),
        "{id} is not in upgrade::MIGRATIONS"
    );
    run_once(record, id, run)
}

fn run_once(record: &Path, id: &str, run: impl FnOnce() -> bool) -> bool {
    if lines(record).iter().any(|line| line == id) {
        return true;
    }
    if !run() {
        return false;
    }
    add(record, &[id.to_owned()]);
    true
}

/// The lines of `path`, none when it is missing.
pub fn lines(path: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.lines().map(str::to_owned).collect()
}

/// Each of `new` among the lines of `path`, written when one is new.
pub fn add(path: &Path, new: &[String]) {
    let mut all = lines(path);
    let before = all.len();
    for line in new {
        if !all.contains(line) {
            all.push(line.clone());
        }
    }
    if all.len() > before {
        let text: String = all.iter().map(|line| format!("{line}\n")).collect();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = agent_client::files::replace_mode(path, text.as_bytes(), 0o644);
    }
}

/// `a.b.c` as numbers, a part that is not one as 0.
#[cfg(test)]
fn version(text: &str) -> [u64; 3] {
    let mut parts = text.split('.').map(|part| part.parse().unwrap_or(0));
    [(); 3].map(|_| parts.next().unwrap_or(0))
}

/// Each step and migration past its release, with what to do about it.
#[cfg(test)]
fn overdue(now: &str, steps: &[Step], migrations: &[Migration]) -> Vec<String> {
    let now = version(now);
    let steps = steps
        .iter()
        .map(|s| (s.id, s.retire, "STEPS and its file in sonny/upgrades"));
    let migrations =
        (migrations.iter()).map(|m| (m.id, m.retire, "MIGRATIONS and the code that runs it"));
    steps
        .chain(migrations)
        .filter(|(_, retire, _)| version(retire) <= now)
        .map(|(id, retire, what)| {
            format!("{id} retires at {retire}: delete it from upgrade::{what}")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_judge_set_in_the_env_file_is_a_step_until_it_goes() {
        let dir = std::env::temp_dir().join(format!("judge-env-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("env");
        assert!(no_judge_in(&file), "no file, nothing to do");
        std::fs::write(&file, "OPENAI_API_KEY=k\nAGENT_JUDGE=openai/x\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(!no_judge_in(&file));
        std::fs::write(&file, "OPENAI_API_KEY=k\n").unwrap();
        assert!(no_judge_in(&file));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_handle_in_the_form_before_messages_had_numbers_is_found() {
        assert!(names_old_handle("waited on turn:calc.fix/48 for it"));
        assert!(names_old_handle("(turn:a/1)"));
        assert!(!names_old_handle("calc.fix/48 and its turn: 3"));
        assert!(!names_old_handle("turn:/4, turn: a/4, turn:a/b"));
        let dir = std::env::temp_dir().join(format!("old-handles-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("projects/one")).unwrap();
        std::fs::write(dir.join("projects/one/fact.md"), "see calc/2").unwrap();
        assert!(!old_handle_under(&dir, 0));
        std::fs::write(dir.join("projects/one/note.txt"), "turn:calc/2").unwrap();
        assert!(!old_handle_under(&dir, 0), "only Markdown is read");
        std::fs::write(dir.join("projects/one/fact.md"), "see turn:calc/2").unwrap();
        assert!(old_handle_under(&dir, 0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nothing_outlives_its_release() {
        let overdue = overdue(env!("CARGO_PKG_VERSION"), STEPS, MIGRATIONS);
        assert!(overdue.is_empty(), "{}", overdue.join("\n"));
    }

    #[test]
    fn a_release_retires_what_names_it() {
        let step = Step {
            id: "move",
            text: "Feature: Commons\nMove it.",
            done: || true,
            retire: "0.9.0",
        };
        let migration = Migration {
            id: "copy",
            retire: "0.10.0",
        };
        let steps = [step];
        let migrations = [migration];
        assert!(overdue("0.8.3", &steps, &migrations).is_empty());
        assert_eq!(
            overdue("0.9.0", &steps, &migrations),
            ["move retires at 0.9.0: delete it from upgrade::STEPS and its file in sonny/upgrades"]
        );
        assert_eq!(overdue("1.0.0", &steps, &migrations).len(), 2);
        // Ten after nine, not before it.
        assert!(version("0.10.0") > version("0.9.0"));
    }

    #[test]
    fn every_step_names_its_feature_and_says_what_to_do() {
        let features = include_str!("../FEATURES.md");
        let mut ids = std::collections::HashSet::new();
        for step in STEPS {
            assert!(ids.insert(step.id), "{} twice", step.id);
            let feature = step.feature().unwrap_or_else(|| {
                panic!("sonny/upgrades/{}.md starts with `Feature: ROW`", step.id)
            });
            assert!(
                features.contains(&format!("| {feature}")),
                "{}: no feature register row starts with {feature:?}",
                step.id
            );
            assert!(
                features.contains(&format!("upgrade step `{}`", step.id)),
                "{}: its row's history names it as upgrade step `{}`",
                step.id,
                step.id
            );
            assert!(!step.body().is_empty(), "{} says what to do", step.id);
        }
        let mut ids = std::collections::HashSet::new();
        for migration in MIGRATIONS {
            assert!(ids.insert(migration.id), "{} twice", migration.id);
        }
    }

    #[test]
    fn a_migration_runs_once_and_again_after_failing() {
        let dir = std::env::temp_dir().join(format!("sonny-once-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let record = dir.join("migrated");
        let run = |answer: bool| {
            let ran = std::cell::Cell::new(false);
            let done = run_once(&record, "copy", || {
                ran.set(true);
                answer
            });
            (ran.get(), done)
        };
        assert_eq!(run(false), (true, false));
        assert_eq!(run(true), (true, true));
        assert_eq!(run(true), (false, true));
        assert_eq!(lines(&record), ["copy"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
