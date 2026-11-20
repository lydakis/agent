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
    // A reviewer model set as `AGENT_JUDGE` in ~/.agent/keys/env, unread since
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
];

/// Whether `~/.agent/keys/env` sets no `AGENT_JUDGE`, the reviewer's model
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
    // Sonny's scheduler job moves to a label that names no one (0.9.0).
    Migration {
        id: crate::scheduler::RELABELED,
        retire: "0.11.0",
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
    // Messages older Sonnys stored with a `[Sent …]` line after the words
    // draw without it (`stamp::unstamped`), now that the daemon's line
    // carries the time (0.10.0).
    Migration {
        id: "sent-stamp",
        retire: "0.12.0",
    },
    // A follow's `bots` become its `agents`, in the Rulebook's words
    // (0.10.0).
    Migration {
        id: crate::lookout::subs::AGENTS,
        retire: "0.12.0",
    },
    // `[sonny] steps` and `verbose`, two switches, become one `view`
    // (0.10.0).
    Migration {
        id: crate::config::VIEW_ONE,
        retire: "0.12.0",
    },
    // `[sonny] task_windows` becomes `most_task_tabs`, and `side_chat =
    // "fork"` `"work"` (0.10.0).
    Migration {
        id: crate::config::TAB_WORDS,
        retire: "0.12.0",
    },
    // `sonny update` is `sonny upgrade`; an older Sonny that installed
    // this one still hands off as `sonny update --installed` (0.10.0).
    Migration {
        id: "update-hand-off",
        retire: "0.11.0",
    },
    // A project file's `threads_*` keys become `task_*`, once for each
    // file, before Sonny first reads it (`crate::project::read`) (0.10.0).
    Migration {
        id: agent_client::project::TASK_KEYS,
        retire: "0.12.0",
    },
    // A project file's `coordinator` key becomes `lead`, as `TASK_KEYS`
    // (0.10.0).
    Migration {
        id: agent_client::project::LEAD_KEY,
        retire: "0.12.0",
    },
    // Each commons post written before posts named their project names
    // the one its author is named in (after 0.10.0).
    Migration {
        id: crate::commons::PROJECTS,
        retire: "0.12.0",
    },
    // What posting in a commons thread subscribed outlives a close, marked
    // so in each follows file (0.10.2).
    Migration {
        id: crate::lookout::subs::COMMONS_OUTLIVE_CLOSE,
        retire: "0.12.0",
    },
];

/// The migrations of Sonny's own files beside `store` and in this HOME,
/// at a window's start and `sonny upgrade`'s, before either starts the
/// daemon or its companions on this build. Once done, each costs a read
/// of its record.
pub fn at_start(store: &Path) {
    store_files(store);
    if let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) {
        crate::commons::woken_gone(&home);
    }
}

/// [`at_start`]'s migrations of the files beside `store`.
fn store_files(store: &Path) {
    crate::lookout::subs::agents_files(store);
    crate::lookout::subs::goals_gone(store);
    crate::lookout::subs::commons_outlive_close(store);
    crate::git::drop_snapshot_indexes(store);
}

/// The migrations that need a whole listing of the store's agents, at a
/// window's start, off its way: none listed once each is done. One window
/// of the store runs them at a time, holding a lock beside its record, so
/// windows that open together list the agents once between them; one that
/// stops short leaves them to the next window that starts.
pub fn from_listing(client: std::sync::Arc<agent_client::Client>, store: &Path) {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    migrate_from_listing(client, store, home);
}

/// [`from_listing`] for the person whose HOME is `home`; the task that
/// runs them, when this window does.
fn migrate_from_listing(
    client: std::sync::Arc<agent_client::Client>,
    store: &Path,
    home: Option<std::path::PathBuf>,
) -> Option<tokio::task::JoinHandle<()>> {
    let record = beside(store);
    let migrated = home.as_ref().map(|home| home.join(".agent/sonny/migrated"));
    let left = |record: &Path| {
        let nested = !done(record, crate::approvals::NESTED_GONE);
        let projects = (migrated.as_ref()).is_some_and(|it| !done(it, crate::commons::PROJECTS));
        (nested, projects)
    };
    if left(&record) == (false, false) {
        return None;
    }
    let Ok(Some(held)) = crate::scheduler::locked(&lock(&record)) else {
        return None;
    };
    // Read again under the lock: a window that held it may have just
    // finished them.
    let (nested, projects) = left(&record);
    if !nested && !projects {
        return None;
    }
    let store = store.to_path_buf();
    Some(tokio::spawn(async move {
        let _held = held;
        let Some(bots) = listing(&client).await else {
            return;
        };
        // Old commons posts name their authors' projects: each thread is
        // rewritten, off the loop.
        let named = (home.filter(|_| projects)).map(|home| {
            let records: std::collections::HashMap<String, serde_json::Value> = (bots.iter())
                .filter_map(|bot| {
                    let name = bot["name"].as_str()?.to_owned();
                    let kept = serde_json::json!({"name": name, "bot_id": bot["bot_id"],
                        "created_by": bot["created_by"], "created_by_id": bot["created_by_id"]});
                    Some((name, kept))
                })
                .collect();
            tokio::task::spawn_blocking(move || crate::commons::projects(&home, &records))
        });
        // What Sonny's walk of the repositories inside agents' folders
        // gave them leaves their sandboxes.
        if nested {
            crate::approvals::nested_gone(&client, &bots, &store).await;
        }
        if let Some(named) = named {
            let _ = named.await;
        }
    }))
}

/// What the migrations read of each agent the daemon lists.
const LISTED: [&str; 5] = ["name", "bot_id", "workspace", "created_by", "created_by_id"];

/// Every agent the daemon lists, with only what the migrations read of
/// each ([`LISTED`]), each page let go once read; none when a page could
/// not be read.
async fn listing(client: &agent_client::Client) -> Option<Vec<serde_json::Value>> {
    use serde_json::Value;
    let mut bots = Vec::new();
    let mut after = Value::Null;
    loop {
        let mut page = client
            .request("bots", serde_json::json!({"limit": 256, "after": after}))
            .await
            .ok()?;
        let Value::Array(listed) = page["bots"].take() else {
            return None;
        };
        bots.extend(listed.into_iter().map(|mut bot| {
            let kept = (LISTED.iter())
                .filter_map(|field| Some(((*field).to_owned(), bot.get_mut(*field)?.take())))
                .collect();
            Value::Object(kept)
        }));
        match page["next_after"].take() {
            Value::Null => return Some(bots),
            next => after = next,
        }
    }
}

/// The lock the window running [`from_listing`]'s migrations holds, beside
/// their `record`.
fn lock(record: &Path) -> std::path::PathBuf {
    let mut lock = record.as_os_str().to_owned();
    lock.push(".lock");
    lock.into()
}

/// The record beside `store` of what was done once for it.
fn beside(store: &Path) -> std::path::PathBuf {
    let mut record = store.as_os_str().to_owned();
    record.push(".migrated");
    record.into()
}

/// Whether `record` says `id` is done.
fn done(record: &Path, id: &str) -> bool {
    lines(record).iter().any(|line| line == id)
}

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

/// `once` for each `what` the migration `id` changes on its own, such as
/// each project file: recorded as `ID WHAT`.
pub fn once_for(record: &Path, id: &str, what: &str, run: impl FnOnce() -> bool) -> bool {
    debug_assert!(
        MIGRATIONS.iter().any(|m| m.id == id),
        "{id} is not in upgrade::MIGRATIONS"
    );
    run_once(record, &format!("{id} {what}"), run)
}

fn run_once(record: &Path, id: &str, run: impl FnOnce() -> bool) -> bool {
    if done(record, id) {
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
    use serde_json::json;

    /// The listing the migrations read keeps only what they read of each
    /// agent, over every page.
    #[tokio::test]
    async fn the_migrations_listing_keeps_only_what_they_read() {
        let client = crate::tests::daemon_answering(|request| {
            let bot = |name: &str, id: i64| {
                json!({"name": name, "bot_id": id, "workspace": "/w", "created_by": "p.lead",
                    "created_by_id": 1, "status": "idle", "instructions": "x".repeat(4096),
                    "last": {"turn": 3, "status": "done"}})
            };
            match request["after"].as_str() {
                None => json!({"bots": [bot("p.a", 2), bot("p.b", 3)], "next_after": "p.b"}),
                Some(_) => json!({"bots": [bot("p.c", 4)], "next_after": null}),
            }
        })
        .await;
        let bots = listing(&client).await.unwrap();
        assert_eq!(bots.len(), 3);
        assert_eq!(
            bots[2],
            json!({"name": "p.c", "bot_id": 4, "workspace": "/w", "created_by": "p.lead",
                "created_by_id": 1})
        );
    }

    /// Windows that open together list the agents for the migrations once
    /// between them: one runs them while the others leave them, and none
    /// lists them again once they are done.
    #[tokio::test]
    async fn one_window_at_a_time_runs_the_migrations_from_a_listing() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let dir = std::env::temp_dir().join(format!("sonny-from-listing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("home")).unwrap();
        let store = dir.join("state.sqlite");
        let listed = std::sync::Arc::new(AtomicUsize::new(0));
        let counted = listed.clone();
        let client = crate::tests::daemon_answering(move |request| {
            if request["op"] == "bots" {
                counted.fetch_add(1, Ordering::SeqCst);
            }
            json!({"bots": [{"name": "p.a", "bot_id": 2}], "next_after": null})
        })
        .await;
        let home = Some(dir.join("home"));
        // Another window runs them.
        let held = crate::scheduler::locked(&lock(&beside(&store)))
            .unwrap()
            .unwrap();
        assert!(migrate_from_listing(client.clone(), &store, home.clone()).is_none());
        drop(held);
        let ran = migrate_from_listing(client.clone(), &store, home.clone()).unwrap();
        ran.await.unwrap();
        assert_eq!(listed.load(Ordering::SeqCst), 1);
        assert!(done(&beside(&store), crate::approvals::NESTED_GONE));
        assert!(done(
            &dir.join("home/.agent/sonny/migrated"),
            crate::commons::PROJECTS
        ));
        assert!(migrate_from_listing(client, &store, home).is_none());
        assert_eq!(listed.load(Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The lookout passes events on and does nothing else (AGENTS.md): no
    /// migration runs in it, nor any of the work they do.
    #[test]
    fn the_lookout_runs_no_migration() {
        let files = [
            ("mod.rs", include_str!("lookout/mod.rs")),
            ("sources.rs", include_str!("lookout/sources.rs")),
            ("activity.rs", include_str!("lookout/activity.rs")),
            ("memory.rs", include_str!("lookout/memory.rs")),
            ("pattern.rs", include_str!("lookout/pattern.rs")),
        ];
        let named = [
            "upgrade::",
            "agents_files",
            "goals_gone",
            "commons_outlive_close",
            "nested_gone",
            "woken_gone",
            "commons::projects",
            "drop_snapshot_indexes",
        ];
        for (file, text) in files {
            for name in named {
                assert!(!text.contains(name), "lookout/{file} runs {name}");
            }
        }
    }

    /// The store's file migrations run at Sonny's start, once each.
    #[test]
    fn the_store_s_files_migrate_at_start() {
        let store = std::env::temp_dir().join(format!("sonny-at-start-{}", std::process::id()));
        let record = beside(&store);
        let mut indexes = store.as_os_str().to_owned();
        indexes.push(".snapshot-indexes");
        let goals = crate::lookout::subs::dir(&store).join("goals");
        std::fs::create_dir_all(&goals).unwrap();
        std::fs::create_dir_all(&indexes).unwrap();
        let _ = std::fs::remove_file(&record);
        store_files(&store);
        assert!(!goals.exists());
        assert!(!Path::new(&indexes).exists());
        for id in [
            crate::lookout::subs::AGENTS,
            crate::lookout::subs::GOALS_GONE,
            crate::lookout::subs::COMMONS_OUTLIVE_CLOSE,
            crate::git::NO_SNAPSHOTS,
        ] {
            assert!(done(&record, id), "{id}");
        }
        let _ = std::fs::remove_dir_all(crate::lookout::subs::dir(&store));
        let _ = std::fs::remove_file(&record);
    }

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

    #[test]
    fn a_project_files_keys_are_renamed_once_and_not_again_after_a_checkout() {
        use agent_client::project::{FILE, TASK_KEYS, rename_task_keys};
        let dir = std::env::temp_dir().join(format!("sonny-task-keys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (record, project) = (dir.join("migrated"), dir.join("shop"));
        std::fs::create_dir_all(project.join(".agents")).unwrap();
        let file = project.join(FILE);
        let what = file.to_string_lossy().into_owned();
        let old = "name = \"shop\"\nthreads_in = \"project\"\n";
        let rename = || once_for(&record, TASK_KEYS, &what, || rename_task_keys(&project));
        // Nothing to rename is not recorded, so a file written later is.
        std::fs::write(&file, "name = \"shop\"\n").unwrap();
        assert!(!rename());
        std::fs::write(&file, old).unwrap();
        assert!(rename());
        let renamed = "name = \"shop\"\ntask_in = \"project\"\n";
        assert_eq!(std::fs::read_to_string(&file).unwrap(), renamed);
        assert_eq!(lines(&record), [format!("{TASK_KEYS} {what}")]);
        // A checkout that brings the old keys back is left for the read to
        // refuse by name.
        std::fs::write(&file, old).unwrap();
        assert!(rename());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), old);
        let error = agent_client::project::read(&project).unwrap_err();
        assert!(error.contains("it is called task_in now"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
