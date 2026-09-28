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
    let unreadable = |error: std::io::Error| {
        Error::with("models_unreadable", &format!("{}: {error}", path.display()))
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
    let text = String::from_utf8(bytes).map_err(|error| {
        Error::with(
            "models_unreadable",
            &format!("{}: {}", path.display(), error.utf8_error()),
        )
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

/// A list from the daemon's `provider_models` answer: every model each
/// provider listed, with what it said about it as the note, and a comment for
/// a provider that listed nothing. A provider that failed keeps its lines
/// from `kept`, the list being replaced, so one provider down never empties
/// the list; a provider no longer running loses them. Refuses output with no
/// usable model lines, including listings dropped to keep the file within
/// its limit.
pub fn render(listing: &Value, kept: &[Model]) -> Result<String, Error> {
    let mut text = String::from(
        "# PROVIDER/MODEL per line; clients offer these. Delete what you don't use.\n",
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
}
