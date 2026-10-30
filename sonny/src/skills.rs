//! Sonny's skills and roles, written into the person's `~/.agents/skills`
//! and `~/.agents/agents`, where every agent finds them and the person can
//! edit them, and the scripts in `~/.agent` they run.
use std::path::{Path, PathBuf};

/// What every agent of Sonny's leans on: the lead's and tasks' roles (and
/// the superlead's, which a project opts into), the
/// memory, plan, commons and automation skills, the nightly cleanup's, and
/// `~/.agent/memory` and `~/.agent/trigger`, which run this Sonny. Written
/// at each start, so they lead to this copy.
pub fn install_own() {
    install_role("lead", crate::project::LEAD);
    install_role("task", crate::project::TASK);
    install_role("superlead", crate::project::SUPERLEAD);
    install("memory", include_str!("../skills/memory/SKILL.md"));
    install(
        "memory-cleanup",
        include_str!("../skills/memory-cleanup/SKILL.md"),
    );
    install("plan", include_str!("../skills/plan/SKILL.md"));
    install("commons", include_str!("../skills/commons/SKILL.md"));
    // A job reaches the daemon the way every agent Sonny runs does.
    let automation = include_str!("../skills/automation/SKILL.md");
    install("automation", &agent_client::policy::with_cli(automation));
    let (Some(home), Some(sonny)) = (std::env::var_os("HOME"), sonny_path()) else {
        return;
    };
    let state = PathBuf::from(home).join(".agent");
    for (name, usage) in [
        ("memory", agent_client::memory::usage()),
        (crate::trigger::VERB, crate::trigger::USAGE),
    ] {
        if let Err(error) = write_runner(&state.join(name), usage, &sonny, name) {
            eprintln!("sonny: {error}");
        }
    }
    // An older Sonny's `~/.agent/project`, for what the new_project tool
    // does now.
    let project = state.join("project");
    if std::fs::read_to_string(&project).is_ok_and(|text| {
        text.starts_with("#!/bin/sh\n# usage: project add") && text.ends_with(" project \"$@\"\n")
    }) {
        let _ = std::fs::remove_file(&project);
    }
}

/// A skill of Sonny's in the person's skills, written as a normal skill:
/// rewritten when Sonny's text changed, never over the person's edits.
pub fn install(name: &str, text: &str) -> Option<PathBuf> {
    install_files(name, &[("SKILL.md", text, 0o644)])
}

/// A skill's files, each with its mode.
fn install_files(name: &str, files: &[(&str, &str, u32)]) -> Option<PathBuf> {
    install_in(&PathBuf::from(std::env::var_os("HOME")?), name, files)
}

/// Where Sonny notes what it last wrote of each skill: a line of
/// `FINGERPRINT FILE` for each file.
fn records(home: &Path) -> PathBuf {
    home.join(".agent/sonny/skills")
}

/// The fingerprints Sonny last wrote of `name`'s files, if it noted any.
fn record(home: &Path, name: &str) -> Option<Vec<(String, String)>> {
    let text = std::fs::read_to_string(records(home).join(name)).ok()?;
    Some(
        text.lines()
            .filter_map(|line| {
                let (hash, file) = line.split_once(' ')?;
                Some((file.to_owned(), hash.to_owned()))
            })
            .collect(),
    )
}

/// Whether the skill at `dir` is Sonny's to change: missing, or as Sonny
/// last wrote it. One Sonny never noted writing is the person's.
fn sonnys(home: &Path, name: &str, dir: &Path) -> bool {
    if !dir.exists() {
        return true;
    }
    match record(home, name) {
        Some(files) => files.iter().all(|(file, hash)| {
            std::fs::read_to_string(dir.join(file))
                .is_ok_and(|now| crate::project::fingerprint(&now) == *hash)
        }),
        None => false,
    }
}

/// `files` as the skill `name` in `home`'s skills, rewritten when one
/// changed and the skill is still Sonny's. A link the app left there,
/// into its bundle, is replaced, never written through; a link of the
/// person's own is left alone.
fn install_in(home: &Path, name: &str, files: &[(&str, &str, u32)]) -> Option<PathBuf> {
    let dir = home.join(".agents/skills").join(name);
    let skill = dir.join("SKILL.md");
    if let Ok(target) = std::fs::read_link(&dir) {
        if !in_app_bundle(&target) {
            return None;
        }
        std::fs::remove_file(&dir).ok()?;
    }
    let same = files.iter().all(|(file, text, _)| {
        std::fs::read_to_string(dir.join(file)).is_ok_and(|now| now == *text)
    });
    let wrote = same || sonnys(home, name, &dir);
    let texts: Vec<(&str, &str)> = files.iter().map(|(file, text, _)| (*file, *text)).collect();
    // The person's edit of a skill Sonny wrote, and Sonny's text changed
    // since: the new text goes beside it, for them to bring in.
    let fingerprints = |files: Vec<(String, String)>| {
        let mut files = files;
        files.sort();
        files
    };
    let shipping = fingerprints(
        (texts.iter())
            .map(|(file, text)| (file.to_string(), crate::project::fingerprint(text)))
            .collect(),
    );
    if !wrote && record(home, name).is_some_and(|last| fingerprints(last) != shipping) {
        stage(home, Kind::Skill, name, &texts);
    }
    if !same && wrote {
        std::fs::create_dir_all(&dir).ok()?;
        for (file, text, mode) in files {
            agent_client::files::replace_mode(&dir.join(file), text.as_bytes(), *mode).ok()?;
        }
        // A file Sonny wrote before and no longer ships goes.
        for (file, _) in record(home, name).unwrap_or_default() {
            if !files.iter().any(|(kept, _, _)| *kept == file) && !file.contains('/') {
                let _ = std::fs::remove_file(dir.join(file));
            }
        }
    }
    if wrote {
        let noted: String = files
            .iter()
            .map(|(file, text, _)| format!("{} {file}\n", crate::project::fingerprint(text)))
            .collect();
        let path = records(home).join(name);
        if std::fs::read_to_string(&path).ok().as_deref() != Some(noted.as_str()) {
            let _ = std::fs::create_dir_all(records(home));
            let _ = agent_client::files::replace_mode(&path, noted.as_bytes(), 0o644);
        }
        keep(&copy(&shipped(home), Kind::Skill, name), &texts);
        unstage(home, Kind::Skill, name);
    }
    (!same && wrote).then_some(skill)
}

/// Whether a link leads into `NAME.app/Contents/Resources/skills`, where
/// the app linked the skills it shipped.
fn in_app_bundle(target: &Path) -> bool {
    let mut up = target.ancestors().skip(1);
    let named = |path: Option<&Path>, name: &str| {
        path.and_then(Path::file_name)
            .is_some_and(|file| file == name)
    };
    named(up.next(), "skills")
        && named(up.next(), "Resources")
        && named(up.next(), "Contents")
        && up
            .next()
            .and_then(Path::extension)
            .is_some_and(|extension| extension == "app")
}

/// This Sonny, by the path that outlives an upgrade: Homebrew's `opt`
/// link for a Sonny in its Cellar, which a scheduled fire still finds after
/// the old version is cleaned up.
pub fn sonny_path() -> Option<PathBuf> {
    let me = std::env::current_exe().ok()?;
    Some(opt_path(&std::fs::canonicalize(&me).unwrap_or(me)))
}

/// `PREFIX/Cellar/NAME/VERSION/REST` as `PREFIX/opt/NAME/REST` when that
/// leads to a file; any other path as it is.
fn opt_path(exe: &Path) -> PathBuf {
    let parts: Vec<_> = exe.components().collect();
    let found = parts
        .iter()
        .position(|part| part.as_os_str() == "Cellar")
        .filter(|at| at + 3 < parts.len());
    let Some(at) = found else {
        return exe.to_owned();
    };
    let mut opt: PathBuf = parts[..at].iter().collect();
    opt.push("opt");
    opt.push(parts[at + 1]);
    opt.extend(&parts[at + 3..]);
    if opt.is_file() { opt } else { exe.to_owned() }
}

/// A script at `path` that runs `sonny` with `verb`, its usage in
/// comments: written when it differs, so a start that changes nothing
/// writes nothing.
fn write_runner(path: &Path, usage: &str, sonny: &Path, verb: &str) -> Result<(), String> {
    let quote = |p: &Path| format!("'{}'", p.to_string_lossy().replace('\'', r"'\''"));
    let usage = usage.replace('\n', "\n# ");
    let text = format!(
        "#!/bin/sh\n# {usage}\nexec {} {verb} \"$@\"\n",
        quote(sonny)
    );
    use std::os::unix::fs::PermissionsExt;
    let runnable = std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o777 == 0o755);
    if runnable && std::fs::read_to_string(path).is_ok_and(|have| have == text) {
        return Ok(());
    }
    agent_client::files::replace_mode(path, text.as_bytes(), 0o755)
}

/// A role of Sonny's as `~/.agents/agents/NAME.md`, written as skills
/// are: rewritten when Sonny's text changed, never over the person's
/// edits, and written again once the person deletes it.
pub fn install_role(name: &str, text: &str) -> Option<PathBuf> {
    install_role_in(&PathBuf::from(std::env::var_os("HOME")?), name, text)
}

/// `text` as the role `name` in `home`, when the file is missing or as
/// Sonny last noted writing it, in `~/.agent/sonny/agents/NAME`. A file
/// Sonny never noted, or a link, is the person's.
fn install_role_in(home: &Path, name: &str, text: &str) -> Option<PathBuf> {
    let path = home.join(".agents/agents").join(format!("{name}.md"));
    let record = home.join(".agent/sonny/agents").join(name);
    let noted = format!("{}\n", crate::project::fingerprint(text));
    let now = match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_symlink() => return None,
        Ok(_) => Some(std::fs::read_to_string(&path).ok()?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return None,
    };
    let same = now.as_deref() == Some(text);
    let wrote = same
        || now.is_none_or(|now| {
            std::fs::read_to_string(&record)
                .is_ok_and(|key| key.trim() == crate::project::fingerprint(&now))
        });
    if !wrote {
        // The person's edit of a role Sonny wrote, and Sonny's text changed
        // since: the new text goes beside it, for them to bring in.
        if std::fs::read_to_string(&record)
            .is_ok_and(|key| key.trim() != crate::project::fingerprint(text))
        {
            stage(home, Kind::Role, name, &[(&format!("{name}.md"), text)]);
        }
        return None;
    }
    keep(
        &copy(&shipped(home), Kind::Role, name),
        &[(&format!("{name}.md"), text)],
    );
    unstage(home, Kind::Role, name);
    if !same {
        std::fs::create_dir_all(path.parent()?).ok()?;
        agent_client::files::replace_mode(&path, text.as_bytes(), 0o644).ok()?;
    }
    if std::fs::read_to_string(&record).ok().as_deref() != Some(noted.as_str()) {
        std::fs::create_dir_all(record.parent()?).ok()?;
        agent_client::files::replace_mode(&record, noted.as_bytes(), 0o644).ok()?;
    }
    (!same).then_some(path)
}

/// A skill or a role, in `~/.agents` and in Sonny's copies of them.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Skill,
    Role,
}

impl Kind {
    fn word(self) -> &'static str {
        match self {
            Kind::Skill => "skill",
            Kind::Role => "role",
        }
    }

    fn parse(word: &str) -> Option<Kind> {
        match word {
            "skill" => Some(Kind::Skill),
            "role" => Some(Kind::Role),
            _ => None,
        }
    }
}

/// How Sonny's note to a bot of its upgrade starts; the transcript shows
/// it as one quiet row.
pub const UPGRADED: &str = "Sonny upgraded: ";

/// What sent that note, for the daemon's record of the turn.
pub const UPGRADE_ORIGIN: &str = "sonny.upgrade";

/// Where Sonny sets its new text of a skill or role the person edited.
fn upgrades(home: &Path) -> PathBuf {
    home.join(".agent/sonny/upgrades")
}

/// Where Sonny keeps whole what it last wrote of each, which is what an
/// edited one started from.
fn shipped(home: &Path) -> PathBuf {
    home.join(".agent/sonny/shipped")
}

/// The skills and roles set beside the person's edits that no bot was
/// told of yet: an empty file named `KIND NAME` for each, so Sonnys adding
/// and claiming at once never write over each other.
fn pending(home: &Path) -> PathBuf {
    upgrades(home).join("pending")
}

/// Where `name` goes under `root`, as in `~/.agents`: a skill's folder,
/// or the folder holding a role's file.
fn copy(root: &Path, kind: Kind, name: &str) -> PathBuf {
    match kind {
        Kind::Skill => root.join("skills").join(name),
        Kind::Role => root.join("agents"),
    }
}

/// Where the person's own `name` is.
fn theirs(home: &Path, kind: Kind, name: &str) -> PathBuf {
    match kind {
        Kind::Skill => home.join(".agents/skills").join(name),
        Kind::Role => home.join(".agents/agents").join(format!("{name}.md")),
    }
}

/// `path` as a note names it: a skill's folder, a role's file.
fn named(dir: &Path, kind: Kind, name: &str) -> PathBuf {
    match kind {
        Kind::Skill => dir.to_owned(),
        Kind::Role => dir.join(format!("{name}.md")),
    }
}

/// Whether `dir` holds each of `files` as it is.
fn holds(dir: &Path, files: &[(&str, &str)]) -> bool {
    files
        .iter()
        .all(|(file, text)| std::fs::read_to_string(dir.join(file)).is_ok_and(|now| now == *text))
}

/// `files` in `dir`, written when one differs.
fn keep(dir: &Path, files: &[(&str, &str)]) -> bool {
    if holds(dir, files) {
        return false;
    }
    let _ = std::fs::create_dir_all(dir);
    files.iter().all(|(file, text)| {
        agent_client::files::replace_mode(&dir.join(file), text.as_bytes(), 0o644).is_ok()
    })
}

/// Sonny's new text of `name` beside the person's edit of it, and noted
/// for the next bot Sonny opens, once for each new text.
fn stage(home: &Path, kind: Kind, name: &str, files: &[(&str, &str)]) {
    let dir = copy(&upgrades(home), kind, name);
    if holds(&dir, files) {
        return;
    }
    // A skill's older new text goes whole, with a file Sonny ships no more.
    if kind == Kind::Skill {
        let _ = std::fs::remove_dir_all(&dir);
    }
    if !keep(&dir, files) {
        return;
    }
    queue(home, &format!("{} {name}", kind.word()));
}

/// `entry` among the pending ones.
fn queue(home: &Path, entry: &str) {
    let _ = std::fs::create_dir_all(pending(home));
    let _ = std::fs::write(pending(home).join(entry), "");
}

/// Every entry in `from` moved to `to`, each by one rename, so each goes
/// to one Sonny alone; the names of those moved.
fn take(from: &Path, to: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(from) else {
        return Vec::new();
    };
    let _ = std::fs::create_dir_all(to);
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| !name.starts_with('.'))
        .filter(|name| std::fs::rename(from.join(name), to.join(name)).is_ok())
        .collect();
    names.sort();
    names
}

/// Where each Sonny moves the entries it tells its bot of, a folder named
/// by its process.
fn claims(home: &Path) -> PathBuf {
    upgrades(home).join("claims")
}

/// How long a Sonny's claim holds before another takes it back: it tells
/// its bot within seconds of starting.
const CLAIM: std::time::Duration = std::time::Duration::from_secs(120);

/// Sonny's new text of `name` gone from beside it, as Sonny writes its own
/// again.
fn unstage(home: &Path, kind: Kind, name: &str) {
    let staged = named(&copy(&upgrades(home), kind, name), kind, name);
    let _ = match kind {
        Kind::Skill => std::fs::remove_dir_all(staged),
        Kind::Role => std::fs::remove_file(staged),
    };
}

/// What Sonny's upgrade set beside the person's edits, for the next bot
/// Sonny opens to tell the person of and offer to bring in.
pub struct Upgrades {
    home: PathBuf,
    /// The pending ones, moved aside so that one Sonny of several starting
    /// at once tells its bot.
    claim: PathBuf,
    note: String,
    key: String,
}

/// The skills and roles Sonny set its new text beside, that no bot was
/// told of yet and the person has not brought in.
pub fn upgraded() -> Option<Upgrades> {
    upgraded_in(&PathBuf::from(std::env::var_os("HOME")?))
}

fn upgraded_in(home: &Path) -> Option<Upgrades> {
    let me = std::process::id().to_string();
    // A claim left by a Sonny that stopped before telling goes back.
    for entry in std::fs::read_dir(claims(home))
        .into_iter()
        .flatten()
        .flatten()
    {
        let stale = entry.file_name() != me.as_str()
            && (entry.metadata().and_then(|m| m.modified()).ok())
                .and_then(|at| at.elapsed().ok())
                .is_some_and(|age| age > CLAIM);
        if stale {
            take(&entry.path(), &pending(home));
            let _ = std::fs::remove_dir(entry.path());
        }
    }
    // A folder made now, so its age is this claim's.
    let claim = claims(home).join(&me);
    let taken = take(&pending(home), &claim);
    if taken.is_empty() {
        let _ = std::fs::remove_dir(&claim);
        return None;
    }
    let mut lines = Vec::new();
    let mut key = String::new();
    for line in &taken {
        let Some((kind, name)) = line.split_once(' ') else {
            continue;
        };
        let Some(kind) = Kind::parse(kind) else {
            continue;
        };
        let staged = named(&copy(&upgrades(home), kind, name), kind, name);
        let mine = theirs(home, kind, name);
        let new = staged_files(home, kind, name);
        let new: Vec<(&str, &str)> = new.iter().map(|(f, t)| (f.as_str(), t.as_str())).collect();
        // Brought in already, or Sonny's new text gone.
        if new.is_empty() || holds(&copy(&home.join(".agents"), kind, name), &new) {
            continue;
        }
        for (file, text) in &new {
            key.push_str(&format!("{file}\n{text}\n"));
        }
        let before = named(&copy(&shipped(home), kind, name), kind, name);
        let before = match before.exists() {
            true => format!(
                "Sonny's previous text, which they started from, at {}",
                before.display()
            ),
            false => "Sonny's previous text was not kept".to_owned(),
        };
        lines.push(format!(
            "- the {name} {}: theirs at {}; Sonny's new text at {}; {before}",
            kind.word(),
            mine.display(),
            staged.display(),
        ));
    }
    let note = (!lines.is_empty()).then(|| {
        format!(
            "{UPGRADED}this Sonny ships new text for skills or roles of its own that the person \
             edited, so it left their copies as they were and set its new text beside them.\n{}\n\
             Tell the person, and ask whether they want you to bring Sonny's changes into their \
             copies. If they do, keep what they changed and take what Sonny changed; where both \
             changed the same part, ask them. A copy left the same as Sonny's new text gets \
             Sonny's updates again from then on.",
            lines.join("\n")
        )
    });
    Some(Upgrades {
        home: home.to_owned(),
        claim,
        key: format!(
            "sonny-upgrade-{}",
            crate::project::fingerprint(&format!("{note:?}{key}"))
        ),
        note: note.unwrap_or_default(),
    })
}

/// Sonny's new text of `name`, each file by its name.
fn staged_files(home: &Path, kind: Kind, name: &str) -> Vec<(String, String)> {
    let dir = copy(&upgrades(home), kind, name);
    let read = |file: String| Some((file.clone(), std::fs::read_to_string(dir.join(&file)).ok()?));
    let mut files: Vec<(String, String)> = match kind {
        Kind::Role => read(format!("{name}.md")).into_iter().collect(),
        Kind::Skill => std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .filter(|file| !file.starts_with('.'))
            .filter_map(read)
            .collect(),
    };
    files.sort();
    files
}

impl Upgrades {
    /// The note for the bot, empty when the person brought each in already.
    pub fn note(&self) -> &str {
        &self.note
    }

    /// The same note twice is one turn.
    pub fn request_id(&self) -> &str {
        &self.key
    }

    /// Each told of, so the next bot is not told again; one Sonny sets
    /// beside them from now is.
    pub fn told(&self) {
        let _ = std::fs::remove_dir_all(&self.claim);
    }

    /// Each back for the next bot, as this one was not told.
    pub fn back(&self) {
        take(&self.claim, &pending(&self.home));
        let _ = std::fs::remove_dir(&self.claim);
    }
}

/// Take a skill of Sonny's out of the person's skills, unless the person
/// edited it.
pub fn remove(name: &str) {
    if let Some(home) = std::env::var_os("HOME") {
        remove_in(&PathBuf::from(home), name);
    }
}

fn remove_in(home: &Path, name: &str) {
    let dir = home.join(".agents/skills").join(name);
    if std::fs::read_link(&dir).is_ok() || !sonnys(home, name, &dir) {
        return;
    }
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_file(records(home).join(name));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_runner_runs_this_sonny_with_its_verb() {
        let dir = std::env::temp_dir().join(format!("sonny-runner-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("memory");
        let sonny = Path::new("/opt/it's/sonny");
        write_runner(
            &path,
            "usage: memory show\n       memory rm",
            sonny,
            "memory",
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            "#!/bin/sh\n# usage: memory show\n#        memory rm\nexec '/opt/it'\\''s/sonny' memory \"$@\"\n"
        );
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o755);
        // Its text survived a crash before its mode did.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_runner(
            &path,
            "usage: memory show\n       memory rm",
            sonny,
            "memory",
        )
        .unwrap();
        assert_eq!(mode(&path), 0o755);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_sonny_in_homebrews_cellar_goes_by_its_opt_link() {
        let root = std::env::temp_dir().join(format!("sonny-opt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let cellar = root.join("Cellar/sonny/0.3.2/libexec/sonny");
        std::fs::create_dir_all(cellar.parent().unwrap()).unwrap();
        std::fs::write(&cellar, "").unwrap();
        // Without the link, the path itself.
        assert_eq!(opt_path(&cellar), cellar);
        let opt = root.join("opt/sonny/libexec/sonny");
        std::fs::create_dir_all(opt.parent().unwrap()).unwrap();
        std::fs::write(&opt, "").unwrap();
        assert_eq!(opt_path(&cellar), opt);
        assert_eq!(
            opt_path(Path::new("/usr/local/bin/sonny")),
            Path::new("/usr/local/bin/sonny")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_file_sonny_no_longer_ships_goes_with_its_next_text() {
        let home = std::env::temp_dir().join(format!("sonny-skills-gone-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let dir = home.join(".agents/skills/plan");
        install_in(
            &home,
            "plan",
            &[("SKILL.md", "one", 0o644), ("plan", "#!/bin/sh", 0o755)],
        );
        assert!(dir.join("plan").exists());
        install_in(&home, "plan", &[("SKILL.md", "two", 0o644)]);
        assert!(!dir.join("plan").exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("SKILL.md")).unwrap(),
            "two"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_skill_is_rewritten_until_the_person_edits_it() {
        let home = std::env::temp_dir().join(format!("sonny-skills-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let skill = home.join(".agents/skills/plan/SKILL.md");
        let read = || std::fs::read_to_string(&skill).unwrap();
        let files = |text| [("SKILL.md", text, 0o644)];
        assert_eq!(
            install_in(&home, "plan", &files("one")),
            Some(skill.clone())
        );
        assert_eq!(read(), "one");
        // Sonny's next text replaces its own.
        assert!(install_in(&home, "plan", &files("two")).is_some());
        assert_eq!(read(), "two");
        // The person's edit stays, through Sonny's next text and a remove.
        std::fs::write(&skill, "mine").unwrap();
        assert!(install_in(&home, "plan", &files("three")).is_none());
        assert_eq!(read(), "mine");
        remove_in(&home, "plan");
        assert_eq!(read(), "mine");
        // A skill of the person's own of that name, which Sonny never wrote, too.
        let theirs = home.join(".agents/skills/notes/SKILL.md");
        std::fs::create_dir_all(theirs.parent().unwrap()).unwrap();
        std::fs::write(&theirs, "theirs").unwrap();
        assert!(install_in(&home, "notes", &files("sonny's")).is_none());
        assert_eq!(std::fs::read_to_string(&theirs).unwrap(), "theirs");
        remove_in(&home, "notes");
        assert_eq!(std::fs::read_to_string(&theirs).unwrap(), "theirs");
        // Once it is gone, Sonny's is written, and an unedited one goes on a remove.
        std::fs::remove_dir_all(theirs.parent().unwrap()).unwrap();
        assert!(install_in(&home, "notes", &files("sonny's")).is_some());
        assert_eq!(std::fs::read_to_string(&theirs).unwrap(), "sonny's");
        remove_in(&home, "notes");
        assert!(!theirs.exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_role_is_rewritten_until_the_person_edits_it() {
        let home = std::env::temp_dir().join(format!("sonny-roles-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let role = home.join(".agents/agents/lead.md");
        let read = || std::fs::read_to_string(&role).unwrap();
        assert_eq!(install_role_in(&home, "lead", "one"), Some(role.clone()));
        assert_eq!(read(), "one");
        // The same text writes nothing.
        assert_eq!(install_role_in(&home, "lead", "one"), None);
        // Sonny's next text replaces its own.
        assert!(install_role_in(&home, "lead", "two").is_some());
        assert_eq!(read(), "two");
        // The person's edit stays through Sonny's next text.
        std::fs::write(&role, "mine").unwrap();
        assert!(install_role_in(&home, "lead", "three").is_none());
        assert_eq!(read(), "mine");
        // Deleted, Sonny's comes back.
        std::fs::remove_file(&role).unwrap();
        assert!(install_role_in(&home, "lead", "three").is_some());
        assert_eq!(read(), "three");
        // A role of the person's own of that name, which Sonny never noted, stays.
        let theirs = home.join(".agents/agents/task.md");
        std::fs::write(&theirs, "theirs").unwrap();
        assert!(install_role_in(&home, "task", "sonny's").is_none());
        assert_eq!(std::fs::read_to_string(&theirs).unwrap(), "theirs");
        // So does a link, wherever it leads.
        let linked = home.join(".agents/agents/linked.md");
        std::os::unix::fs::symlink(&theirs, &linked).unwrap();
        assert!(install_role_in(&home, "linked", "sonny's").is_none());
        assert_eq!(std::fs::read_to_string(&theirs).unwrap(), "theirs");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn sonnys_new_text_goes_beside_an_edit_for_the_next_bot_to_offer() {
        let home = std::env::temp_dir().join(format!("sonny-upgrades-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let role = home.join(".agents/agents/lead.md");
        let skill = home.join(".agents/skills/plan/SKILL.md");
        let files = |text| [("SKILL.md", text, 0o644)];
        install_role_in(&home, "lead", "one");
        install_in(&home, "plan", &files("one"));
        // Nothing to tell while each is Sonny's.
        install_role_in(&home, "lead", "two");
        assert!(upgraded_in(&home).is_none());
        // The person edits both; the same Sonny starting again has nothing new.
        std::fs::write(&role, "two, mine").unwrap();
        std::fs::write(&skill, "one, mine").unwrap();
        install_role_in(&home, "lead", "two");
        install_in(&home, "plan", &files("one"));
        assert!(upgraded_in(&home).is_none());
        // Sonny's next text goes beside them.
        assert!(install_role_in(&home, "lead", "three").is_none());
        assert!(install_in(&home, "plan", &files("two")).is_none());
        assert_eq!(std::fs::read_to_string(&role).unwrap(), "two, mine");
        let staged = home.join(".agent/sonny/upgrades/agents/lead.md");
        assert_eq!(std::fs::read_to_string(&staged).unwrap(), "three");
        let upgrades = upgraded_in(&home).unwrap();
        // Claimed: a second Sonny starting at once has none to tell.
        assert!(upgraded_in(&home).is_none());
        let note = upgrades.note();
        assert!(note.starts_with(UPGRADED), "{note}");
        let before = home.join(".agent/sonny/shipped/agents/lead.md");
        assert_eq!(std::fs::read_to_string(&before).unwrap(), "two");
        assert!(
            note.contains(&format!(
                "- the lead role: theirs at {}; Sonny's new text at {}; Sonny's previous text, which they started from, at {}",
                role.display(),
                staged.display(),
                before.display()
            )),
            "{note}"
        );
        assert!(note.contains("- the plan skill: theirs at "), "{note}");
        // Not told, they wait for the next bot.
        upgrades.back();
        let upgrades = upgraded_in(&home).unwrap();
        assert!(upgrades.note().contains("the plan skill"));
        // Told once; the same text at the next start is not told again.
        upgrades.told();
        install_role_in(&home, "lead", "three");
        install_in(&home, "plan", &files("two"));
        assert!(upgraded_in(&home).is_none());
        // Sonny's next text is, under its own key.
        install_role_in(&home, "lead", "four");
        let again = upgraded_in(&home).unwrap();
        assert!(again.note().contains("the lead role"));
        assert!(!again.note().contains("the plan skill"));
        assert_ne!(again.request_id(), upgrades.request_id());
        again.back();
        // Brought in before the bot heard: nothing to say, and cleared.
        std::fs::write(&role, "four").unwrap();
        let brought = upgraded_in(&home).unwrap();
        assert_eq!(brought.note(), "");
        brought.told();
        assert!(upgraded_in(&home).is_none());
        // A claim a Sonny left as it stopped goes back after a while.
        install_role_in(&home, "lead", "five, offered");
        let left = home.join(".agent/sonny/upgrades/claims/1");
        take(&pending(&home), &left);
        assert!(upgraded_in(&home).is_none());
        let old = std::time::SystemTime::now() - CLAIM * 2;
        std::fs::File::open(&left)
            .unwrap()
            .set_modified(old)
            .unwrap();
        upgraded_in(&home).unwrap().told();
        assert!(!left.exists());
        std::fs::write(&role, "four").unwrap();
        // Sonny's own again at the next start, beside nothing, it takes
        // Sonny's next text.
        assert!(install_role_in(&home, "lead", "four").is_none());
        assert!(!staged.exists());
        assert!(install_role_in(&home, "lead", "five").is_some());
        assert_eq!(std::fs::read_to_string(&role).unwrap(), "five");
        // A file of the person's own that Sonny never wrote is not offered.
        std::fs::write(home.join(".agents/agents/task.md"), "theirs").unwrap();
        install_role_in(&home, "task", "sonny's");
        assert!(upgraded_in(&home).is_none());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn only_the_apps_links_are_replaced() {
        assert!(in_app_bundle(Path::new(
            "/Applications/Agent.app/Contents/Resources/skills/memory"
        )));
        assert!(!in_app_bundle(Path::new("/Users/g/skills/memory")));
        assert!(!in_app_bundle(Path::new("memory")));
    }
}
