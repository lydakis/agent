//! One-off work an upgrade brings, each retired two releases after it
//! ships. What needs judgment, or reaches what is the person's own (config
//! they wrote, skills they changed, hosts this machine knows), is a step:
//! words in `sonny/upgrades/ID.md` that the upgrade note hands the folder's
//! lead, as release notes hand an operator, with a check in code that says
//! when it is done. What a script does the same everywhere (a rewrite of
//! Sonny's own files) is a migration: code that runs once, through
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
pub const STEPS: &[Step] = &[];

/// Something Sonny does once itself, by [`once`], for as long as it is
/// listed.
pub struct Migration {
    pub id: &'static str,
    /// The release it is deleted in, two after the one it ships in: a test
    /// fails from then on.
    #[cfg_attr(not(test), allow(dead_code))]
    pub retire: &'static str,
}

/// Every migration [`once`] runs, so none outlives its release.
pub const MIGRATIONS: &[Migration] = &[Migration {
    // Ships in 0.8.0.
    id: crate::commons::POSTERS,
    retire: "0.10.0",
}];

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
