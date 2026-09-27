//! The model list: `~/.agent/models`, one `PROVIDER/MODEL` per line, with
//! anything after `#` a note. Clients read it to offer choices: the app's
//! model picker, and `agent models`, which is how a bot sees what it may give
//! the agents it starts. The daemon never reads it and runs whatever model it
//! is given. `agent models --discover` writes a first one from the
//! providers' own listings; after that it is the user's file.
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
    use std::io::Read;
    let unreadable = |error: std::io::Error| Error {
        code: "models_unreadable".into(),
        detail: Some(format!("{}: {error}", path.display())),
    };
    // Opened without blocking, so a FIFO put in its place is refused below
    // rather than waited on; reads of a regular file are unaffected.
    let opened = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
            .open(path)
    };
    let file = match opened {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
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
    let text = String::from_utf8(bytes).map_err(|error| Error {
        code: "models_unreadable".into(),
        detail: Some(format!("{}: {}", path.display(), error.utf8_error())),
    })?;
    parse(&text).map_err(|(line, reason)| {
        Error::with(
            "models_invalid",
            &format!("{}:{line}: {reason}", path.display()),
        )
    })
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

/// A first list from the daemon's `provider_models` answer: every model each
/// provider listed, with what it said about it as the note, and a comment for
/// a provider that listed nothing.
pub fn render(listing: &Value) -> String {
    let mut text = String::from(
        "# PROVIDER/MODEL per line; clients offer these. Delete what you don't use.\n",
    );
    let Some(providers) = listing["providers"].as_object() else {
        return text;
    };
    let mut names: Vec<&String> = providers.keys().collect();
    names.sort();
    for name in names {
        let listed = &providers[name];
        text.push('\n');
        let Some(models) = listed["models"].as_array() else {
            let error = listed["error"].as_str().unwrap_or("no listing");
            let said = match listed["detail"].as_str() {
                Some(detail) => format!("{name}: {error}: {detail}"),
                None => format!("{name}: {error}"),
            };
            // One comment line, whatever line breaks the provider sent.
            let said: Vec<&str> = said.split_whitespace().collect();
            text.push_str(&format!("# {}\n", said.join(" ")));
            continue;
        };
        let mut lines = String::new();
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
            // `#` starts the note, so it cannot appear inside one's text either way.
            let note = note.join(", ").replace('\n', " ");
            match note.is_empty() {
                true => lines.push_str(&format!("{id}\n")),
                false => lines.push_str(&format!("{id}  # {note}\n")),
            }
        }
        // A provider that answered but gave nothing usable says so by name,
        // and one that would make the list too long to read back is left out.
        if lines.is_empty() {
            text.push_str(&format!("# {name}: no models listed\n"));
        } else if text.len() + lines.len() + 128 > LIMIT as usize {
            text.push_str(&format!(
                "# {name}: {} bytes of models, past the list's 1 MiB\n",
                lines.len()
            ));
        } else {
            text.push_str(&lines);
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::{parse, render};
    use serde_json::json;

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
        let text = render(&json!({"providers":{
            "openai":{"models":[{"id":"gpt-6-luna"},{"id":"bad id"}]},
            "anthropic":{"models":[{"id":"claude-sonnet-5","name":"Claude Sonnet 5",
                "context_tokens":1000000,"output_tokens":128000}]},
            "bedrock":{"error":"provider_http_404","detail":"not found"},
            "gone":{"error":"provider_http_500","detail":"down\nother/model\r\nx"},
            "quiet":{"models":[{"id":"bad id"}]}}}));
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
        let text = render(&json!({"providers":{
            "openai":{"models":[{"id":"gpt-6-luna"}]}, long.clone(): {"models": many}}}));
        assert!(text.len() <= super::LIMIT as usize);
        assert!(text.contains(&format!("# {long}: ")));
        let ids: Vec<_> = parse(&text).unwrap().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["openai/gpt-6-luna"]);
    }
}
