//! The model list clients offer, from two files with one owner each.
//! Discovery owns `~/.agent/models.discovered`: the providers' own
//! listings, rewritten whole at each daemon start and by `agent models
//! --discover`, so it says what the providers report now and nothing else.
//! The person owns `~/.agent/models`, which nothing writes: a
//! `PROVIDER/MODEL` line adds a model, or puts a listed one first with
//! their note, and a `-PROVIDER/MODEL` line hides one. In both, one entry
//! per line, with anything after `#` a note. Clients read the two together
//! ([`read`]): the person's lines first, then the catalog's, less what they
//! hide. The daemon never reads either and runs whatever model it is given.
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

/// The person's list.
pub fn path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".agent").join("models"))
}

/// Discovery's catalog beside the person's list at `path`.
pub fn discovered(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".discovered");
    path.with_file_name(name)
}

/// What clients offer: the person's models at `path` in their order, then
/// the catalog's they did not name, less every model they hide; each once.
/// No files is no list.
pub fn read(path: &Path) -> Result<Vec<Model>, Error> {
    let mine = lines_at(path)?;
    // Only discovery writes the catalog, never through a link.
    let catalog = discovered(path);
    let listed = match read_file(&catalog, false)? {
        Some(text) => parse(&text).map_err(invalid(&catalog))?,
        None => Vec::new(),
    };
    let hidden: std::collections::HashSet<&str> = mine
        .iter()
        .filter(|(hide, _)| *hide)
        .map(|(_, model)| model.id.as_str())
        .collect();
    let mut seen: std::collections::HashSet<String> =
        hidden.iter().map(|id| (*id).to_owned()).collect();
    let mut models = Vec::new();
    for model in mine
        .into_iter()
        .filter_map(|(hide, model)| (!hide).then_some(model))
        .chain(listed)
    {
        if seen.insert(model.id.clone()) {
            models.push(model);
        }
    }
    Ok(models)
}

/// The models the person lists at `path`, in their order, less those
/// they hide: their own lines only, none of the catalog's.
pub fn mine(path: &Path) -> Result<Vec<Model>, Error> {
    let listed = lines_at(path)?.into_iter();
    Ok(listed
        .filter_map(|(hide, model)| (!hide).then_some(model))
        .collect())
}

/// The person's lines at `path`, each a model and whether it hides it.
fn lines_at(path: &Path) -> Result<Vec<(bool, Model)>, Error> {
    match read_file(path, true)? {
        Some(text) => lines(&text).map_err(invalid(path)),
        None => Ok(Vec::new()),
    }
}

fn invalid(path: &Path) -> impl Fn((usize, &'static str)) -> Error + '_ {
    move |(line, reason)| {
        Error::with(
            "models_invalid",
            &format!("{}:{line}: {reason}", path.display()),
        )
    }
}

/// A list file's text, or none where there is none: a regular file of at
/// most 1 MiB of UTF-8. The catalog is read with `follow` false, so a link
/// put where discovery writes is refused rather than read through.
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

/// The models the person's list at `path` hides, for a client that offers
/// what the providers list now rather than [`read`]'s list; none when it
/// cannot be read.
pub fn hidden(path: &Path) -> std::collections::HashSet<String> {
    read_file(path, true)
        .ok()
        .flatten()
        .and_then(|text| lines(&text).ok())
        .into_iter()
        .flatten()
        .filter_map(|(hide, model)| hide.then_some(model.id))
        .collect()
}

/// The models a list names, hides left out.
fn parse(text: &str) -> Result<Vec<Model>, (usize, &'static str)> {
    Ok(lines(text)?
        .into_iter()
        .filter_map(|(hide, model)| (!hide).then_some(model))
        .collect())
}

/// Each entry in file order, as whether it hides (`-PROVIDER/MODEL`) and
/// the model it names; an entry repeated counts once.
fn lines(text: &str) -> Result<Vec<(bool, Model)>, (usize, &'static str)> {
    let mut models = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (index, line) in text.lines().enumerate() {
        let (entry, note) = match line.split_once('#') {
            Some((entry, note)) => (entry.trim(), Some(note.trim())),
            None => (line.trim(), None),
        };
        if entry.is_empty() {
            continue;
        }
        let (hide, entry) = match entry.strip_prefix('-') {
            Some(entry) => (true, entry.trim_start()),
            None => (false, entry),
        };
        let valid = entry.split_once('/').is_some_and(|(provider, model)| {
            (1..=64).contains(&provider.len())
                && provider
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                && !provider.starts_with('-')
                && (1..=256).contains(&model.len())
                && !model.contains(char::is_whitespace)
        });
        if !valid {
            return Err((index + 1, "expected PROVIDER/MODEL or -PROVIDER/MODEL"));
        }
        if !seen.insert((hide, entry)) {
            continue;
        }
        models.push((
            hide,
            Model {
                id: entry.to_owned(),
                note: note.filter(|note| !note.is_empty()).map(str::to_owned),
            },
        ));
    }
    Ok(models)
}

/// A catalog from the daemon's `provider_models` answer: every model each
/// provider listed, with what it said about it as the note, and a comment
/// for a provider that listed nothing. A provider that failed keeps its
/// lines from `kept`, the catalog being replaced, so one provider down never
/// empties it; a provider no longer running loses them. Refuses output with no
/// usable model lines, including listings dropped to keep the file within
/// its limit.
pub fn render(listing: &Value, kept: &[Model]) -> Result<String, Error> {
    let mut text = String::from(
        "# What the providers list, rewritten whole at each daemon start; edits here\n\
         # are replaced. Add a model, or hide one with -PROVIDER/MODEL, in `models`.\n",
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

/// What a refresh did to the catalog: models added and removed, whether it
/// wrote a first one, and each provider that did not answer, with why.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Refreshed {
    pub created: bool,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub refused: Vec<(String, String)>,
}

/// Rewrites the catalog at `path` whole from the daemon's
/// `provider_models` answer. A provider that did not answer keeps its
/// lines from the catalog there now; nothing else in it is read, so an
/// edit there is replaced. Written beside it and renamed over it, so a
/// reader sees the old catalog or the new one, never part of either.
pub fn refresh(path: &Path, listing: &Value) -> Result<Refreshed, Error> {
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
    // A catalog that is missing, a link or broken is simply replaced.
    let old = read_file(path, false).ok().flatten();
    let kept = old
        .as_deref()
        .and_then(|text| parse(text).ok())
        .unwrap_or_default();
    let text = render(listing, &kept)?;
    let now = parse(&text).unwrap_or_default();
    let had: std::collections::HashSet<&str> = kept.iter().map(|m| m.id.as_str()).collect();
    let has: std::collections::HashSet<&str> = now.iter().map(|m| m.id.as_str()).collect();
    let refreshed = Refreshed {
        created: old.is_none(),
        added: now
            .iter()
            .filter(|m| !had.contains(m.id.as_str()))
            .map(|m| m.id.clone())
            .collect(),
        removed: kept
            .iter()
            .filter(|m| !has.contains(m.id.as_str()))
            .map(|m| m.id.clone())
            .collect(),
        refused,
    };
    if old.as_deref() != Some(text.as_str()) {
        let staged = stage(path, &text)?;
        commit(&staged, path)?;
    }
    Ok(refreshed)
}

fn folder(path: &Path) -> &Path {
    path.parent()
        .filter(|folder| !folder.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
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

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("models-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ids(models: &[super::Model]) -> Vec<&str> {
        models.iter().map(|m| m.id.as_str()).collect()
    }

    #[test]
    fn the_persons_own_models_leave_out_the_catalog() {
        let dir = temp("mine");
        let list = dir.join("models");
        std::fs::write(&list, "openai/a  # fast\n-openai/b\nopenai/c\n").unwrap();
        std::fs::write(
            super::discovered(&list),
            "openai/b  # B\nopenai/d  # D, 200k tokens\n",
        )
        .unwrap();
        let mine = super::mine(&list).unwrap();
        assert_eq!(ids(&mine), ["openai/a", "openai/c"]);
        assert_eq!(mine[0].note.as_deref(), Some("fast"));
        // What clients offer still reads both.
        assert_eq!(
            ids(&super::read(&list).unwrap()),
            ["openai/a", "openai/c", "openai/d"]
        );
        // No list of the person's is no models, whatever the catalog holds.
        std::fs::remove_file(&list).unwrap();
        assert!(super::mine(&list).unwrap().is_empty());
    }

    #[test]
    fn each_refresh_rewrites_the_catalog_whole_from_what_the_providers_list() {
        let dir = temp("catalog");
        let catalog = dir.join("models.discovered");
        let first = super::refresh(
            &catalog,
            &json!({"providers":{
                "openai":{"models":[{"id":"a"},{"id":"b"}]},
                "bedrock":{"models":[{"id":"x","name":"X"}]}}}),
        )
        .unwrap();
        assert!(first.created);
        // An edit to the catalog does not survive the next refresh.
        let text = std::fs::read_to_string(&catalog).unwrap();
        std::fs::write(&catalog, text.replace("openai/a\n", "openai/a  # mine\n")).unwrap();
        // b is gone, c is new; bedrock is down and keeps its lines.
        let second = super::refresh(
            &catalog,
            &json!({"providers":{
                "openai":{"models":[{"id":"a"},{"id":"c"}]},
                "bedrock":{"error":"provider_aws_credentials_expired"}}}),
        )
        .unwrap();
        assert_eq!(
            second,
            super::Refreshed {
                created: false,
                added: vec!["openai/c".into()],
                removed: vec!["openai/b".into()],
                refused: vec![("bedrock".into(), "provider_aws_credentials_expired".into())],
            }
        );
        let after = std::fs::read_to_string(&catalog).unwrap();
        let models = parse(&after).unwrap();
        assert_eq!(ids(&models), ["bedrock/x", "openai/a", "openai/c"]);
        assert_eq!(models[0].note.as_deref(), Some("X"));
        assert_eq!(models[1].note, None, "{after}");
        // The same listing again leaves the file as it is.
        let again = super::refresh(
            &catalog,
            &json!({"providers":{"openai":{"models":[{"id":"a"},{"id":"c"}]},
                "bedrock":{"error":"provider_aws_credentials_expired"}}}),
        )
        .unwrap();
        assert!(again.added.is_empty() && again.removed.is_empty());
        assert_eq!(std::fs::read_to_string(&catalog).unwrap(), after);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn clients_read_the_person_s_models_first_then_the_catalog_less_what_they_hide() {
        let dir = temp("mine");
        let list = dir.join("models");
        super::refresh(
            &super::discovered(&list),
            &json!({"providers":{"openai":{"models":[
                {"id":"a","name":"A"},{"id":"b"},{"id":"c"}]}}}),
        )
        .unwrap();
        assert_eq!(
            ids(&super::read(&list).unwrap()),
            ["openai/a", "openai/b", "openai/c"]
        );
        std::fs::write(
            &list,
            "# mine\nopenai/c  # for reviews\n-openai/b\nanthropic/by-hand\n- openai/gone\n",
        )
        .unwrap();
        let models = super::read(&list).unwrap();
        assert_eq!(ids(&models), ["openai/c", "anthropic/by-hand", "openai/a"]);
        assert_eq!(models[0].note.as_deref(), Some("for reviews"));
        assert_eq!(models[2].note.as_deref(), Some("A"));
        // A refresh never touches the person's file.
        super::refresh(
            &super::discovered(&list),
            &json!({"providers":{"openai":{"models":[{"id":"d"}]}}}),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&list).unwrap(),
            "# mine\nopenai/c  # for reviews\n-openai/b\nanthropic/by-hand\n- openai/gone\n"
        );
        assert_eq!(
            ids(&super::read(&list).unwrap()),
            ["openai/c", "anthropic/by-hand", "openai/d"]
        );
        // Neither file is no list.
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(super::read(&list).unwrap().is_empty());
    }

    #[test]
    fn a_hide_names_one_model_and_a_bad_line_is_refused_by_number() {
        let models = super::lines("-openai/a\nopenai/a\n-openai/a\n").unwrap();
        let listed: Vec<_> = models
            .iter()
            .map(|(hide, m)| (*hide, m.id.as_str()))
            .collect();
        assert_eq!(listed, [(true, "openai/a"), (false, "openai/a")]);
        assert_eq!(super::lines("ok/a\n--openai/a\n").unwrap_err().0, 2);
        assert_eq!(super::lines("-\n").unwrap_err().0, 1);
        assert_eq!(ids(&parse("-openai/a\nopenai/b\n").unwrap()), ["openai/b"]);
    }

    #[test]
    fn a_link_put_where_the_catalog_goes_is_replaced_never_written_through() {
        let dir = temp("planted");
        let catalog = dir.join("models.discovered");
        let victim = dir.join("victim");
        std::fs::write(&victim, "keep/this\n").unwrap();
        std::os::unix::fs::symlink(&victim, &catalog).unwrap();
        let listing = json!({"providers":{"openai":{"models":[{"id":"a"}]}}});
        let refreshed = super::refresh(&catalog, &listing).unwrap();
        // The link is not read through either: nothing it named is kept.
        assert!(refreshed.created);
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep/this\n");
        assert!(
            !std::fs::symlink_metadata(&catalog)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            ids(&parse(&std::fs::read_to_string(&catalog).unwrap()).unwrap()),
            ["openai/a"]
        );
        let left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .filter(|name| name.starts_with('.'))
            .collect();
        assert!(left.is_empty(), "{left:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_refresh_that_lists_nothing_usable_writes_nothing() {
        let dir = temp("none");
        let catalog = dir.join("models.discovered");
        let error = super::refresh(
            &catalog,
            &json!({"providers":{"gone":{"error":"provider_connection"}}}),
        )
        .unwrap_err();
        assert_eq!(error.code, "models_none_listed");
        assert!(!catalog.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
