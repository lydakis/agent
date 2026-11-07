//! The model list: `~/.agent/models`, one `PROVIDER/MODEL` per line, with
//! anything after `#` a note. Clients read it to offer choices: the app's
//! model picker, and `agent models`, which is how a bot sees what it may give
//! the agents it starts. The daemon never reads it and runs whatever model it
//! is given. The daemon keeps it current from the providers' own listings
//! each time it starts, and `agent models --discover` does the same on
//! demand: a three-way merge of the file, the providers' listings now and
//! what refreshes put in the file (`models.listed`, beside it), so a model a
//! provider newly lists is added, one it stopped listing is removed, and a
//! line the person deleted, added or annotated stays as they left it, a
//! deletion even through a time the provider stopped listing the model.
use crate::Error;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// A list longer than this is a mistake, not a list.
const LIMIT: u64 = 1024 * 1024;

#[derive(Debug)]
pub struct Model {
    pub id: String,
    pub note: Option<String>,
}

impl Model {
    pub fn json(&self) -> Value {
        match &self.note {
            Some(note) => json!({"id": self.id, "note": note}),
            None => json!({"id": self.id}),
        }
    }
}

pub fn path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".agent").join("models"))
}

/// The listed models in file order, each once. No file is no list.
pub fn read(path: &Path) -> Result<Vec<Model>, Error> {
    let Some(text) = read_file(path, true)? else {
        return Ok(Vec::new());
    };
    parse(&text).map_err(|(line, reason)| {
        Error::with(
            "models_invalid",
            &format!("{}:{line}: {reason}", path.display()),
        )
    })
}

/// A list file's text, or none where there is none: a regular file of at
/// most 1 MiB of UTF-8. A refresh reads with `follow` false, so a link put
/// where it writes is refused rather than read through.
fn read_file(path: &Path, follow: bool) -> Result<Option<String>, Error> {
    use std::io::Read;
    let unreadable = |error: std::io::Error| {
        Error::with("models_unreadable", &format!("{}: {error}", path.display()))
    };
    // Opened without blocking, so a FIFO put in its place is refused below
    // rather than waited on; reads of a regular file are unaffected.
    let opened = {
        use std::os::unix::fs::OpenOptionsExt;
        let link = if follow { 0 } else { libc::O_NOFOLLOW };
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY | link)
            .open(path)
    };
    let file = match opened {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(unreadable(error)),
    };
    if !file.metadata().map_err(unreadable)?.is_file() {
        return Err(Error::with(
            "models_invalid",
            &format!("{}: not a regular file", path.display()),
        ));
    }
    // Read once and never past the limit, whatever the file became since.
    let mut bytes = Vec::new();
    file.take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(unreadable)?;
    if bytes.len() as u64 > LIMIT {
        return Err(Error::with(
            "models_invalid",
            &format!("{}: larger than 1 MiB", path.display()),
        ));
    }
    String::from_utf8(bytes).map(Some).map_err(|error| {
        Error::with(
            "models_unreadable",
            &format!("{}: {}", path.display(), error.utf8_error()),
        )
    })
}

/// Writes a first list at `path`, whole beside it and then linked into
/// place: a failed write leaves no partial list, and a list made meanwhile
/// is not replaced but refused with `models_file_exists`.
pub fn install(path: &Path, text: &str) -> Result<(), Error> {
    let failed = |error: std::io::Error| match error.kind() {
        std::io::ErrorKind::AlreadyExists => {
            Error::with("models_file_exists", &path.display().to_string())
        }
        _ => Error::with("models_unwritable", &format!("{}: {error}", path.display())),
    };
    let staged = stage(path, text)?;
    let installed = std::fs::hard_link(&staged, path);
    let _ = std::fs::remove_file(&staged);
    installed.map_err(failed)?;
    sync_folder(path)
}

fn parse(text: &str) -> Result<Vec<Model>, (usize, &'static str)> {
    let mut models: Vec<Model> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (index, line) in text.lines().enumerate() {
        let (entry, note) = match line.split_once('#') {
            Some((entry, note)) => (entry.trim(), Some(note.trim())),
            None => (line.trim(), None),
        };
        if entry.is_empty() {
            continue;
        }
        let valid = entry.split_once('/').is_some_and(|(provider, model)| {
            (1..=64).contains(&provider.len())
                && provider
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                && (1..=256).contains(&model.len())
                && !model.contains(char::is_whitespace)
        });
        if !valid {
            return Err((index + 1, "expected PROVIDER/MODEL"));
        }
        if !seen.insert(entry) {
            continue;
        }
        models.push(Model {
            id: entry.to_owned(),
            note: note.filter(|note| !note.is_empty()).map(str::to_owned),
        });
    }
    Ok(models)
}

/// A list from the daemon's `provider_models` answer: every model each
/// provider listed, with what it said about it as the note, and a comment for
/// a provider that listed nothing. A provider that failed keeps its lines
/// from `kept`, the list being replaced, so one provider down never empties
/// the list; a provider no longer running loses them. Refuses output with no
/// usable model lines, including listings dropped to keep the file within
/// its limit.
pub fn render(listing: &Value, kept: &[Model]) -> Result<String, Error> {
    let mut text = String::from(
        "# PROVIDER/MODEL per line; clients offer these. Delete what you don't use.\n\
         # Each daemon start adds models a provider newly lists and removes ones it\n\
         # stopped listing; lines you delete stay deleted, lines you add stay.\n",
    );
    let Some(providers) = listing["providers"].as_object() else {
        return Err(Error::with("models_none_listed", "no provider listings"));
    };
    let mut names: Vec<&String> = providers.keys().collect();
    names.sort();
    // Every block, comments included, is counted against the limit the
    // list is read back at, with room kept for one closing line.
    let room = LIMIT as usize - 96;
    let mut left_out = 0;
    let mut has_models = false;
    for name in names {
        let (block, instead) = provider_block(name, &providers[name], kept);
        match [block, instead]
            .into_iter()
            .flatten()
            .find(|block| text.len() + 1 + block.len() <= room)
        {
            Some(block) => {
                has_models |= block.lines().any(|line| !line.starts_with('#'));
                text.push('\n');
                text.push_str(&block);
            }
            None => left_out += 1,
        }
    }
    if left_out > 0 {
        text.push_str(&format!(
            "\n# {left_out} more providers left out, past the list's 1 MiB\n"
        ));
    }
    if !has_models {
        let refused: Vec<_> = providers
            .iter()
            .map(|(name, listed)| {
                format!(
                    "{name}: {}",
                    listed["error"].as_str().unwrap_or("no usable models")
                )
            })
            .collect();
        return Err(Error::with("models_none_listed", &refused.join(", ")));
    }
    Ok(text)
}

/// A provider's lines, and a shorter comment to write in their place when
/// they do not fit.
fn provider_block(name: &str, listed: &Value, kept: &[Model]) -> (Option<String>, Option<String>) {
    let comment = |said: String| {
        // One comment line, whatever line breaks the provider sent.
        let said: Vec<&str> = said.split_whitespace().collect();
        format!("# {}\n", said.join(" "))
    };
    let Some(models) = listed["models"].as_array() else {
        let error = listed["error"].as_str().unwrap_or("no listing");
        let prefix = format!("{name}/");
        let last: String = kept
            .iter()
            .filter(|model| model.id.starts_with(&prefix))
            .map(|model| match &model.note {
                Some(note) => format!("{}  # {note}\n", model.id),
                None => format!("{}\n", model.id),
            })
            .collect();
        if !last.is_empty() {
            let said = comment(format!("{name}: {error}; the last list is kept"));
            return (Some(format!("{said}{last}")), Some(said));
        }
        let full = listed["detail"]
            .as_str()
            .map(|detail| comment(format!("{name}: {error}: {detail}")));
        return (full, Some(comment(format!("{name}: {error}"))));
    };
    let lines: String = listed_lines(name, models)
        .into_iter()
        .map(|(_, line)| line)
        .collect();
    // A provider that answered but gave nothing usable says so by name.
    if lines.is_empty() {
        return (None, Some(comment(format!("{name}: no models listed"))));
    }
    let over = comment(format!(
        "{name}: {} bytes of models, past the list's 1 MiB",
        lines.len()
    ));
    (Some(lines), Some(over))
}

/// A provider's listed models as `(PROVIDER/MODEL, line)`, in the order it
/// listed them, each line with what the provider said about the model as
/// its note. An id that would not read back as one line is left out.
fn listed_lines(name: &str, models: &[Value]) -> Vec<(String, String)> {
    let mut lines = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for model in models {
        let Some(id) = model["id"].as_str() else {
            continue;
        };
        let mut note = Vec::new();
        if let Some(display) = model["name"].as_str() {
            note.push(display.to_owned());
        }
        if let Some(tokens) = model["context_tokens"].as_u64() {
            note.push(format!("{tokens} context"));
        }
        if let Some(tokens) = model["output_tokens"].as_u64() {
            note.push(format!("{tokens} output"));
        }
        let id = format!("{name}/{id}");
        // A line a model id breaks would not read back; leave it out.
        if !parse(&id).is_ok_and(|parsed| parsed.len() == 1 && parsed[0].id == id) {
            continue;
        }
        if !seen.insert(id.clone()) {
            continue;
        }
        // `#` starts the note, so it cannot appear inside one's text either way.
        let note = note.join(", ").replace(['\n', '\r'], " ");
        let line = match note.is_empty() {
            true => format!("{id}\n"),
            false => format!("{id}  # {note}\n"),
        };
        lines.push((id, line));
    }
    lines
}

/// What a refresh did to the list: models added and removed, whether it
/// wrote a first list, and each provider that did not answer, with why.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Refreshed {
    pub created: bool,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub refused: Vec<(String, String)>,
}

/// Where the refresh keeps what it put in the list: `models.listed`
/// beside the list, one `PROVIDER/MODEL` per line. An id there and not in
/// the list was deleted by hand and is not added back.
pub fn listed_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".listed");
    path.with_file_name(name)
}

/// Brings the list at `path` up to date with the daemon's
/// `provider_models` answer, or writes a first one where there is none.
/// Only providers that answered are touched, so one provider down, or a
/// daemon running other providers, leaves the rest of the list alone.
/// A list someone edits meanwhile is merged again rather than replaced.
pub fn refresh(path: &Path, listing: &Value) -> Result<Refreshed, Error> {
    // A list kept as a link (a dotfiles checkout) is updated where it lives.
    let path = match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            std::fs::canonicalize(path).map_err(|error| {
                Error::with("models_unreadable", &format!("{}: {error}", path.display()))
            })?
        }
        _ => path.to_owned(),
    };
    let path = path.as_path();
    let Some(providers) = listing["providers"].as_object() else {
        return Err(Error::with("models_none_listed", "no provider listings"));
    };
    let refused: Vec<(String, String)> = providers
        .iter()
        .filter(|(_, listed)| !listed["models"].is_array())
        .map(|(name, listed)| {
            let error = listed["error"].as_str().unwrap_or("no listing");
            let said = match listed["detail"].as_str() {
                Some(detail) => format!("{error}: {detail}"),
                None => error.to_owned(),
            };
            (
                name.clone(),
                said.split_whitespace().collect::<Vec<_>>().join(" "),
            )
        })
        .collect();
    let answered: Vec<(&String, Vec<(String, String)>)> = providers
        .iter()
        .filter_map(|(name, listed)| Some((name, listed_lines(name, listed["models"].as_array()?))))
        .collect();
    let record_path = listed_path(path);
    // One refresh of a folder at a time (a daemon starting and `agent
    // models --discover`), so neither installs a merge made from a list
    // the other has since replaced.
    let _held = hold(path)?;
    for _ in 0..4 {
        let text = match read_text(path)? {
            Some(text) => text,
            None => {
                let text = render(listing, &[])?;
                // What the list holds, not every id listed: a provider
                // left out for the size limit is not recorded as deleted.
                let lines = written(&text);
                let added: Vec<String> = lines.keys().cloned().collect();
                let record = record_text(lines, path)?;
                match install(path, &text) {
                    // Someone wrote one meanwhile: merge into theirs.
                    Err(error) if error.code == "models_file_exists" => continue,
                    installed => installed?,
                }
                write_record(&record_path, &record)?;
                return Ok(Refreshed {
                    created: true,
                    added,
                    removed: Vec::new(),
                    refused,
                });
            }
        };
        let record = read_record(&record_path)?;
        let merged = merge(&text, &record, &answered);
        // Built and checked before the list is touched, so a record that
        // could not be written never leaves a list it does not describe.
        let mut next: std::collections::BTreeMap<String, String> = record
            .into_iter()
            .filter(|(id, _)| !merged.removed.contains(id))
            .collect();
        next.extend(merged.listed.clone());
        let next = match record_text(next.clone(), path) {
            Ok(next) => next,
            // Past the limit only through ids deleted by hand long ago:
            // keep only those still listed.
            Err(_) => record_text(
                next.into_iter()
                    .filter(|(id, _)| merged.listed.contains_key(id))
                    .collect(),
                path,
            )?,
        };
        if merged.text != text && !replace(path, &text, &merged.text)? {
            continue;
        }
        write_record(&record_path, &next)?;
        return Ok(Refreshed {
            created: false,
            added: merged.added,
            removed: merged.removed,
            refused,
        });
    }
    Err(Error::with(
        "models_busy",
        &format!("{}: changed during every refresh", path.display()),
    ))
}

/// Holds the list's folder for one refresh, waiting up to ten seconds for
/// another to finish. A folder whose file system has no locks is not held.
fn hold(path: &Path) -> Result<std::fs::File, Error> {
    use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};
    let failed = |error: std::io::Error| {
        Error::with("models_unwritable", &format!("{}: {error}", path.display()))
    };
    let folder = folder(path);
    std::fs::create_dir_all(folder).map_err(failed)?;
    let held = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(folder)
        .map_err(failed)?;
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        // SAFETY: flock on a descriptor `held` owns; closing it lets go.
        if unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(held);
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::WouldBlock {
            return Ok(held);
        }
        if std::time::Instant::now() >= until {
            return Err(Error::with(
                "models_busy",
                &format!("{}: another refresh holds it", path.display()),
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn folder(path: &Path) -> &Path {
    path.parent()
        .filter(|folder| !folder.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

/// The list's text, or none where there is no list; a list that does not
/// parse is refused, never merged into.
fn read_text(path: &Path) -> Result<Option<String>, Error> {
    let Some(text) = read_file(path, false)? else {
        return Ok(None);
    };
    parse(&text).map_err(|(line, reason)| {
        Error::with(
            "models_invalid",
            &format!("{}:{line}: {reason}", path.display()),
        )
    })?;
    Ok(Some(text))
}

/// The lines the refresh put in the list, by model, as it wrote them;
/// none before the first refresh.
fn read_record(path: &Path) -> Result<std::collections::HashMap<String, String>, Error> {
    let Some(text) = read_file(path, false)? else {
        return Ok(Default::default());
    };
    parse(&text).map_err(|(line, reason)| {
        Error::with(
            "models_invalid",
            &format!("{}:{line}: {reason}", path.display()),
        )
    })?;
    Ok(written(&text).into_iter().collect())
}

/// Each model line in `text`, by the model it names, without its line end.
fn written(text: &str) -> std::collections::BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| Some((entry(line)?.to_owned(), line.trim_end().to_owned())))
        .collect()
}

/// The record's text, refused past the size its reader takes.
fn record_text(
    lines: std::collections::BTreeMap<String, String>,
    path: &Path,
) -> Result<String, Error> {
    let mut text = String::from(
        "# The lines the refresh put in the list beside this one. A model here\n\
         # and missing there was deleted by hand and is not added back; a line\n\
         # that differs there was changed by hand and is the person's to keep.\n",
    );
    for line in lines.into_values() {
        text.push_str(&line);
        text.push('\n');
    }
    match text.len() as u64 > LIMIT {
        true => Err(Error::with(
            "models_invalid",
            &format!("{}: its record would be larger than 1 MiB", path.display()),
        )),
        false => Ok(text),
    }
}

fn write_record(path: &Path, text: &str) -> Result<(), Error> {
    match read_file(path, false) {
        Ok(Some(old)) if old == text => Ok(()),
        _ => replace_unchecked(path, text),
    }
}

/// A merge: the list's new text, what it added and removed, and the
/// refresh's own lines the answering providers list now that the new text
/// holds, by model (`listed`).
#[derive(Default)]
struct Merged {
    text: String,
    added: Vec<String>,
    removed: Vec<String>,
    listed: std::collections::HashMap<String, String>,
}

/// The merged text: for each provider that answered, lines the refresh put
/// in and it no longer lists are dropped, with the notes discovery wrote
/// about it failing; models it lists that the list has never had are added
/// after its last line, or as a block at the end, while they fit the size
/// limit. Everything else is kept byte for byte.
fn merge(
    text: &str,
    record: &std::collections::HashMap<String, String>,
    answered: &[(&String, Vec<(String, String)>)],
) -> Merged {
    let provider_of = |id: &str| id.split_once('/').map(|(provider, _)| provider.to_owned());
    let listed: std::collections::HashMap<&str, std::collections::HashSet<&str>> = answered
        .iter()
        .map(|(name, lines)| {
            (
                name.as_str(),
                lines.iter().map(|(id, _)| id.as_str()).collect(),
            )
        })
        .collect();
    let mut merged = Merged::default();
    let mut kept: Vec<(Option<String>, &str)> = Vec::new();
    let mut present = std::collections::HashSet::new();
    for line in text.split_inclusive('\n') {
        let id = entry(line);
        if let Some(id) = id {
            present.insert(id.to_owned());
            let now = provider_of(id).and_then(|provider| listed.get(provider.as_str()));
            // Still the refresh's own line, as it wrote it: one the person
            // wrote or changed stays theirs, kept whatever the provider lists.
            let own = record.get(id).is_some_and(|wrote| wrote == line.trim_end());
            if own && now.is_some_and(|now| !now.contains(id)) {
                merged.removed.push(id.to_owned());
                continue;
            }
            if own && now.is_some_and(|now| now.contains(id)) {
                merged
                    .listed
                    .insert(id.to_owned(), line.trim_end().to_owned());
            }
        } else if answered.iter().any(|(name, _)| failure_note(line, name)) {
            // The provider answers now, so what the first list said about
            // its failure no longer holds.
            continue;
        }
        kept.push((id.and_then(provider_of), line));
    }
    let mut out: Vec<String> = kept.iter().map(|(_, line)| (*line).to_owned()).collect();
    if out.last().is_some_and(|line| !line.ends_with('\n')) {
        out.last_mut().expect("last").push('\n');
    }
    let mut size: usize = out.iter().map(String::len).sum();
    for (name, lines) in answered {
        let new: Vec<&(String, String)> = lines
            .iter()
            .filter(|(id, _)| !present.contains(id) && !record.contains_key(id))
            .collect();
        if new.is_empty() {
            continue;
        }
        let block: String = new.iter().map(|(_, line)| line.as_str()).collect();
        let last = kept
            .iter()
            .rposition(|(provider, _)| provider.as_deref() == Some(name.as_str()));
        let gap = usize::from(last.is_none() && out.last().is_some_and(|l| !l.trim().is_empty()));
        if (size + gap + block.len()) as u64 > LIMIT {
            continue;
        }
        size += gap + block.len();
        merged.added.extend(new.iter().map(|(id, _)| id.clone()));
        merged.listed.extend(
            new.iter()
                .map(|(id, line)| (id.clone(), line.trim_end().to_owned())),
        );
        match last {
            Some(index) => out[index].push_str(&block),
            None => {
                if gap == 1 {
                    out.push("\n".to_owned());
                }
                out.push(block);
                kept.push((Some((*name).clone()), ""));
            }
        }
    }
    merged.text = out.concat();
    merged
}

/// The model a list line names, if it names one.
fn entry(line: &str) -> Option<&str> {
    let entry = line.split_once('#').map_or(line, |(entry, _)| entry).trim();
    (!entry.is_empty()).then_some(entry)
}

/// Whether `line` is one of the comments [`render`] writes when `name`
/// did not answer or listed nothing usable.
fn failure_note(line: &str, name: &str) -> bool {
    let Some(said) = line.trim_end().strip_prefix(&format!("# {name}: ")) else {
        return false;
    };
    said.starts_with("provider_")
        || said.starts_with("models_")
        || said == "no listing"
        || said == "no models listed"
        || said.ends_with("past the list's 1 MiB")
}

/// Replaces the list with `new` if it still reads `old`, whole and in one
/// rename, keeping its permissions; false if it changed meanwhile. The new
/// text is written out first and the list compared last, just before the
/// rename, so an edit saved while it was written is merged, not lost.
fn replace(path: &Path, old: &str, new: &str) -> Result<bool, Error> {
    let staged = stage(path, new)?;
    if !matches!(read_file(path, false), Ok(Some(now)) if now == old) {
        let _ = std::fs::remove_file(&staged);
        return Ok(false);
    }
    commit(&staged, path)?;
    Ok(true)
}

fn replace_unchecked(path: &Path, text: &str) -> Result<(), Error> {
    let staged = stage(path, text)?;
    commit(&staged, path)
}

/// Writes `text` to a new file beside `path`, durable and with the
/// permissions `path` has. The file is made new under a name not used
/// before, never opened through a link or over a file already there, so a
/// name planted in the folder can only be skipped, never written through.
fn stage(path: &Path, text: &str) -> Result<PathBuf, Error> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    static STAGED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let failed = |error: std::io::Error| {
        Error::with("models_unwritable", &format!("{}: {error}", path.display()))
    };
    std::fs::create_dir_all(folder(path)).map_err(failed)?;
    // The permissions of the list there now, never of what a link names.
    let permissions = std::fs::symlink_metadata(path)
        .ok()
        .filter(std::fs::Metadata::is_file)
        .map(|meta| meta.permissions());
    for _ in 0..8 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.subsec_nanos());
        let mut name = std::ffi::OsString::from(".");
        name.push(path.file_name().unwrap_or_default());
        name.push(format!(
            ".{}.{}{nanos:08x}",
            std::process::id(),
            STAGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let staged = path.with_file_name(name);
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&staged)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(failed(error)),
        };
        let written = (|| {
            if let Some(permissions) = &permissions {
                file.set_permissions(permissions.clone())?;
            }
            file.write_all(text.as_bytes())?;
            file.sync_all()
        })();
        if let Err(error) = written {
            let _ = std::fs::remove_file(&staged);
            return Err(failed(error));
        }
        return Ok(staged);
    }
    Err(Error::with(
        "models_unwritable",
        &format!("{}: no free name to write beside it", path.display()),
    ))
}

/// Renames a staged file over `path`, which replaces the name there (a
/// link too) and never writes through it.
fn commit(staged: &Path, path: &Path) -> Result<(), Error> {
    if let Err(error) = std::fs::rename(staged, path) {
        let _ = std::fs::remove_file(staged);
        return Err(Error::with(
            "models_unwritable",
            &format!("{}: {error}", path.display()),
        ));
    }
    sync_folder(path)
}

/// A new name is durable only once its folder is.
fn sync_folder(path: &Path) -> Result<(), Error> {
    std::fs::File::open(folder(path))
        .and_then(|dir| dir.sync_all())
        .map_err(|error| Error::with("models_unwritable", &format!("{}: {error}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::{parse, render};
    use serde_json::json;

    #[test]
    fn discovery_requires_a_model_that_survives_rendering() {
        for models in [
            json!([]),
            json!([{"id":"bad id"}]),
            json!([{"id":"x".repeat(257)}]),
        ] {
            let listing = json!({"providers":{"custom":{"models":models}}});
            assert_eq!(
                render(&listing, &[]).unwrap_err().code,
                "models_none_listed"
            );
        }
        let refused = json!({"providers":{"gone":{"error":"provider_connection"}}});
        assert!(
            render(&refused, &[])
                .unwrap_err()
                .detail
                .unwrap()
                .contains("gone: provider_connection")
        );
        // Valid ids can also be omitted when the rendered block exceeds the file limit.
        let large: Vec<_> = (0..5000)
            .map(|i| json!({"id":format!("m{i}"), "name":"n".repeat(256)}))
            .collect();
        assert_eq!(
            render(&json!({"providers":{"large":{"models":large}}}), &[])
                .unwrap_err()
                .code,
            "models_none_listed"
        );
    }

    #[test]
    fn a_provider_that_fails_keeps_its_last_lines_and_one_that_left_loses_them() {
        let kept = parse(
            "bedrock/claude-opus-5  # Claude Opus 5\nbedrock/claude-haiku-5\ngone/model\nopenai/old\n",
        )
        .unwrap();
        let text = render(
            &json!({"providers":{
                "bedrock":{"error":"provider_aws_credentials_expired"},
                "openai":{"models":[{"id":"gpt-6-luna"}]}}}),
            &kept,
        )
        .unwrap();
        assert!(
            text.contains("# bedrock: provider_aws_credentials_expired; the last list is kept\n")
        );
        let ids: Vec<_> = parse(&text).unwrap().into_iter().map(|m| m.id).collect();
        assert_eq!(
            ids,
            [
                "bedrock/claude-opus-5",
                "bedrock/claude-haiku-5",
                "openai/gpt-6-luna"
            ]
        );
        assert_eq!(
            parse(&text).unwrap()[0].note.as_deref(),
            Some("Claude Opus 5")
        );
        // Kept lines count as usable: a list is not refused while one survives.
        let down = json!({"providers":{"bedrock":{"error":"provider_connection"}}});
        assert!(render(&down, &kept).is_ok());
        assert!(render(&down, &[]).is_err());
    }

    #[test]
    fn lines_are_ids_with_notes_and_comments_and_blanks_are_skipped() {
        let models = parse(
            "# mine\n\nanthropic/claude-sonnet-5  # fast\n openrouter/vendor/model \nanthropic/claude-sonnet-5\n",
        )
        .unwrap();
        let listed: Vec<_> = models
            .iter()
            .map(|m| (m.id.as_str(), m.note.as_deref()))
            .collect();
        assert_eq!(
            listed,
            [
                ("anthropic/claude-sonnet-5", Some("fast")),
                ("openrouter/vendor/model", None)
            ]
        );
        assert_eq!(parse("claude-sonnet-5\n").unwrap_err().0, 1);
        assert_eq!(parse("ok/model\nbad provider/x\n").unwrap_err().0, 2);
        assert_eq!(parse("a/b c\n").unwrap_err().0, 1);
    }

    #[test]
    fn a_discovered_list_reads_back_as_the_models_listed() {
        let text = render(
            &json!({"providers":{
            "openai":{"models":[{"id":"gpt-6-luna"},{"id":"bad id"}]},
            "anthropic":{"models":[{"id":"claude-sonnet-5","name":"Claude Sonnet 5",
                "context_tokens":1000000,"output_tokens":128000}]},
            "bedrock":{"error":"provider_http_404","detail":"not found"},
            "gone":{"error":"provider_http_500","detail":"down\nother/model\r\nx"},
            "quiet":{"models":[{"id":"bad id"}]}}}),
            &[],
        )
        .unwrap();
        assert!(text.contains(
            "anthropic/claude-sonnet-5  # Claude Sonnet 5, 1000000 context, 128000 output\n"
        ));
        assert!(text.contains("# bedrock: provider_http_404: not found\n"));
        assert!(text.contains("# gone: provider_http_500: down other/model x\n"));
        assert!(text.contains("# quiet: no models listed\n"));
        let ids: Vec<_> = parse(&text).unwrap().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["anthropic/claude-sonnet-5", "openai/gpt-6-luna"]);
        let many: Vec<_> = (0..40_000)
            .map(|i| json!({"id": format!("model-{i}")}))
            .collect();
        let long = "p".repeat(64);
        let text = render(
            &json!({"providers":{
            "openai":{"models":[{"id":"gpt-6-luna"}]}, long.clone(): {"models": many}}}),
            &[],
        )
        .unwrap();
        assert!(text.len() <= super::LIMIT as usize);
        assert!(text.contains(&format!("# {long}: ")));
        let ids: Vec<_> = parse(&text).unwrap().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["openai/gpt-6-luna"]);

        // Comments count too: a list that nearly fills the file leaves the
        // rest as one closing line, never past the limit.
        let fill = (super::LIMIT as usize - 6000) / 15;
        let fill: Vec<_> = (0..fill)
            .map(|i| json!({"id": format!("model-{i:06}")}))
            .collect();
        let mut listing = json!({"a": {"models": fill}});
        for i in 0..200 {
            listing[format!("z-{i:03}")] =
                json!({"error": "provider_http_500", "detail": "d".repeat(300)});
        }
        let text = render(&json!({ "providers": listing }), &[]).unwrap();
        assert!(text.len() <= super::LIMIT as usize);
        assert!(text.contains("more providers left out, past the list's 1 MiB\n"));
        assert!(parse(&text).is_ok());
    }

    fn refreshed_in(dir: &std::path::Path, listing: serde_json::Value) -> super::Refreshed {
        super::refresh(&dir.join("models"), &listing).unwrap()
    }

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("models-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_refresh_adds_new_models_drops_gone_ones_and_keeps_the_person_s_edits() {
        let dir = temp("merge");
        let list = dir.join("models");
        let first = refreshed_in(
            &dir,
            json!({"providers":{
                "openai":{"models":[{"id":"a"},{"id":"b"},{"id":"c"}]},
                "bedrock":{"models":[{"id":"x","name":"X"}]}}}),
        );
        assert!(first.created);
        // The person deletes b, annotates a, and adds a model of their own.
        let text = std::fs::read_to_string(&list)
            .unwrap()
            .replace("openai/b\n", "")
            .replace("openai/a\n", "openai/a  # mine\nopenai/hand-added\n");
        std::fs::write(&list, &text).unwrap();
        // c is gone, d is new; bedrock is down this time.
        let second = refreshed_in(
            &dir,
            json!({"providers":{
                "openai":{"models":[{"id":"a","name":"A"},{"id":"b"},{"id":"d"}]},
                "bedrock":{"error":"provider_aws_credentials_expired"}}}),
        );
        assert_eq!(second.added, ["openai/d"]);
        assert_eq!(second.removed, ["openai/c"]);
        assert_eq!(
            second.refused,
            [(
                "bedrock".to_owned(),
                "provider_aws_credentials_expired".to_owned()
            )]
        );
        let after = std::fs::read_to_string(&list).unwrap();
        let models = parse(&after).unwrap();
        let ids: Vec<_> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            ["bedrock/x", "openai/a", "openai/hand-added", "openai/d"]
        );
        assert_eq!(models[1].note.as_deref(), Some("mine"));
        assert!(after.starts_with("# PROVIDER/MODEL per line"), "{after}");
        // Nothing new: the file is left byte for byte, and b stays deleted.
        let third = refreshed_in(
            &dir,
            json!({"providers":{"openai":{"models":[{"id":"a"},{"id":"b"},{"id":"d"}]}}}),
        );
        assert_eq!(third, super::Refreshed::default());
        assert_eq!(std::fs::read_to_string(&list).unwrap(), after);
        // The record keeps bedrock's last listing while it was down.
        let record = std::fs::read_to_string(super::listed_path(&list)).unwrap();
        let recorded = super::written(&record);
        assert!(
            recorded.contains_key("bedrock/x") && recorded.contains_key("openai/b"),
            "{record}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_list_made_before_refreshes_existed_loses_nothing() {
        // No record yet: nothing can be shown to be discovery's, so nothing
        // is removed, and every listed model the list lacks is added once.
        let dir = temp("upgrade");
        let list = dir.join("models");
        std::fs::write(
            &list,
            "# mine\nopenai/old  # kept\n# openai: provider_http_500\n",
        )
        .unwrap();
        let refreshed = refreshed_in(
            &dir,
            json!({"providers":{"openai":{"models":[{"id":"new"}]},
                "anthropic":{"models":[{"id":"claude"}]}}}),
        );
        assert_eq!(refreshed.added, ["anthropic/claude", "openai/new"]);
        assert!(refreshed.removed.is_empty());
        assert_eq!(
            std::fs::read_to_string(&list).unwrap(),
            "# mine\nopenai/old  # kept\nopenai/new\n\nanthropic/claude\n"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_linked_list_is_refreshed_where_it_lives() {
        let dir = temp("link");
        let real = dir.join("dotfiles-models");
        std::fs::write(&real, "openai/a\n").unwrap();
        std::os::unix::fs::symlink(&real, dir.join("models")).unwrap();
        refreshed_in(
            &dir,
            json!({"providers":{"openai":{"models":[{"id":"a"},{"id":"b"}]}}}),
        );
        assert!(
            std::fs::symlink_metadata(dir.join("models"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(&real).unwrap(),
            "openai/a\nopenai/b\n"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_list_that_does_not_parse_is_never_merged_into() {
        let dir = temp("broken");
        std::fs::write(dir.join("models"), "not a model\n").unwrap();
        let error = super::refresh(
            &dir.join("models"),
            &json!({"providers":{"openai":{"models":[{"id":"a"}]}}}),
        )
        .unwrap_err();
        assert_eq!(error.code, "models_invalid");
        assert_eq!(
            std::fs::read_to_string(dir.join("models")).unwrap(),
            "not a model\n"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_model_deleted_by_hand_stays_deleted_through_a_listing_that_drops_it() {
        let dir = temp("marker");
        let list = dir.join("models");
        let both = json!({"providers":{"openai":{"models":[{"id":"a"},{"id":"b"}]}}});
        refreshed_in(&dir, both.clone());
        let text = std::fs::read_to_string(&list)
            .unwrap()
            .replace("openai/b\n", "");
        std::fs::write(&list, &text).unwrap();
        // b leaves the listing for one refresh, then comes back.
        refreshed_in(
            &dir,
            json!({"providers":{"openai":{"models":[{"id":"a"}]}}}),
        );
        let back = refreshed_in(&dir, both);
        assert_eq!(back, super::Refreshed::default());
        assert_eq!(std::fs::read_to_string(&list).unwrap(), text);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_line_annotated_by_hand_stays_when_the_provider_stops_listing_it() {
        let dir = temp("annotated");
        let list = dir.join("models");
        let both = json!({"providers":{"openai":{"models":[{"id":"a"},{"id":"b","name":"B"}]}}});
        refreshed_in(&dir, both.clone());
        let text = std::fs::read_to_string(&list).unwrap();
        let line = text
            .lines()
            .find(|line| line.starts_with("openai/b"))
            .unwrap();
        let text = text.replace(line, "openai/b  # B, the one for reviews");
        std::fs::write(&list, &text).unwrap();
        let gone = refreshed_in(
            &dir,
            json!({"providers":{"openai":{"models":[{"id":"a"}]}}}),
        );
        assert_eq!(gone, super::Refreshed::default());
        assert_eq!(std::fs::read_to_string(&list).unwrap(), text);
        // Listed again, it is not added a second time.
        assert_eq!(refreshed_in(&dir, both), super::Refreshed::default());
        assert_eq!(std::fs::read_to_string(&list).unwrap(), text);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_model_the_provider_drops_and_lists_again_comes_back() {
        let dir = temp("returns");
        let both = json!({"providers":{"openai":{"models":[{"id":"a"},{"id":"b"}]}}});
        refreshed_in(&dir, both.clone());
        let gone = refreshed_in(
            &dir,
            json!({"providers":{"openai":{"models":[{"id":"a"}]}}}),
        );
        assert_eq!(gone.removed, ["openai/b"]);
        let back = refreshed_in(&dir, both);
        assert_eq!(back.added, ["openai/b"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_listing_past_the_size_limit_is_left_out_and_never_recorded() {
        let dir = temp("large");
        let list = dir.join("models");
        let long: Vec<serde_json::Value> = (0..6000)
            .map(|n| json!({"id": format!("{n:0>200}")}))
            .collect();
        let listing = json!({"providers":{
            "big":{"models": long},
            "openai":{"models":[{"id":"a"}]}}});
        let first = refreshed_in(&dir, listing.clone());
        assert_eq!(first.added, ["openai/a"]);
        let record = std::fs::read_to_string(super::listed_path(&list)).unwrap();
        assert!(!record.contains("big/"), "{}", record.len());
        // Later refreshes still read their record, and still leave it out.
        let again = refreshed_in(&dir, listing);
        assert_eq!(again, super::Refreshed::default());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_link_put_where_the_refresh_writes_is_never_written_through() {
        let dir = temp("planted");
        let list = dir.join("models");
        let listing = json!({"providers":{"openai":{"models":[{"id":"a"}]}}});
        refreshed_in(&dir, listing.clone());
        let victim = dir.join("victim");
        std::fs::write(&victim, "keep\n").unwrap();
        let record = super::listed_path(&list);
        std::fs::remove_file(&record).unwrap();
        std::os::unix::fs::symlink(&victim, &record).unwrap();
        let error = super::refresh(&list, &listing).unwrap_err();
        assert_eq!(error.code, "models_unreadable");
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep\n");
        // Nothing staged is left behind either.
        let left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .filter(|name| name.starts_with('.'))
            .collect();
        assert!(left.is_empty(), "{left:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_line_the_person_wrote_stays_when_the_provider_stops_listing_it() {
        let dir = temp("theirs");
        let list = dir.join("models");
        refreshed_in(
            &dir,
            json!({"providers":{"openai":{"models":[{"id":"a"}]}}}),
        );
        // Their own line, for a model the provider also lists for a while.
        let text = std::fs::read_to_string(&list).unwrap() + "openai/mine  # by hand\n";
        std::fs::write(&list, &text).unwrap();
        refreshed_in(
            &dir,
            json!({"providers":{"openai":{"models":[{"id":"a"},{"id":"mine"}]}}}),
        );
        let gone = refreshed_in(
            &dir,
            json!({"providers":{"openai":{"models":[{"id":"a"}]}}}),
        );
        assert!(gone.removed.is_empty(), "{gone:?}");
        assert_eq!(std::fs::read_to_string(&list).unwrap(), text);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
