//! `/model`: the lead's model and effort, picked from what its provider
//! family lists. A bot changes model only within its family, since its
//! history is in that family's shape.
use serde_json::Value;

pub struct Picker {
    /// `provider/id`, as the daemon names a bot's model.
    models: Vec<String>,
    /// Typed to narrow the list.
    pub filter: String,
    pick: usize,
    /// No effort first: the provider's default.
    efforts: Vec<Option<String>>,
    effort: usize,
    /// Why a provider of the family listed nothing.
    pub refused: Vec<String>,
}

impl Picker {
    /// The models of every binding in `current`'s family, from a
    /// `provider_models` answer and `ready.providers`, the current one
    /// picked. A provider that would not list still offers the current
    /// model.
    pub fn new(listing: &Value, providers: &Value, current: &str, effort: Option<&str>) -> Self {
        let provider = current.split_once('/').map_or(current, |(p, _)| p);
        let family = &providers[provider]["family"];
        let mut names: Vec<&String> = providers
            .as_object()
            .map(|all| {
                all.iter()
                    .filter(|(_, binding)| binding["family"] == *family)
                    .map(|(name, _)| name)
                    .collect()
            })
            .unwrap_or_default();
        // The lead's own provider first.
        names.sort_by_key(|name| (name.as_str() != provider, name.as_str()));
        let mut models: Vec<String> = Vec::new();
        let mut refused = Vec::new();
        for name in names {
            let answer = &listing["providers"][name];
            if let Some(error) = answer["error"].as_str() {
                refused.push(format!("{name}: {error}"));
            }
            for model in answer["models"].as_array().into_iter().flatten() {
                if let Some(id) = model["id"].as_str() {
                    models.push(format!("{name}/{id}"));
                }
            }
        }
        if !models.iter().any(|m| m == current) {
            models.insert(0, current.to_owned());
        }
        let pick = models.iter().position(|m| m == current).unwrap_or(0);
        let mut efforts = vec![None];
        efforts.extend(
            providers[provider]["efforts"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(|e| Some(e.to_owned())),
        );
        if let Some(effort) = effort
            && !efforts.iter().any(|e| e.as_deref() == Some(effort))
        {
            efforts.push(Some(effort.to_owned()));
        }
        let effort = efforts
            .iter()
            .position(|e| e.as_deref() == effort)
            .unwrap_or(0);
        Picker {
            models,
            filter: String::new(),
            pick,
            efforts,
            effort,
            refused,
        }
    }

    /// The models the filter leaves, best first.
    pub fn shown(&self) -> Vec<&str> {
        self.found().into_iter().map(|(model, _)| model).collect()
    }

    /// The models the filter leaves, best first, each with where the
    /// letters typed are in its name as `named` gives it. A model is found
    /// by that name or by its id, as `us.anthropic` finds one Bedrock
    /// region's; found by its id, no letters of the name are marked.
    pub fn found(&self) -> Vec<(&str, Vec<usize>)> {
        use crate::fuzzy::{better, find, order};
        let found = self.models.iter().map(|id| {
            let name = find(&named(id), &self.filter);
            let best = better(name.clone(), find(id, &self.filter));
            best.map(|mut best| {
                if name.as_ref() != Some(&best) {
                    best.at.clear();
                }
                best
            })
        });
        (order(found, |_| 0.0).into_iter())
            .map(|(at, found)| (self.models[at].as_str(), found.at))
            .collect()
    }

    /// Where the pick is among `shown()`.
    pub fn at(&self) -> usize {
        self.pick.min(self.shown().len().saturating_sub(1))
    }

    pub fn up(&mut self) {
        let len = self.shown().len().max(1);
        self.pick = (self.at() + len - 1) % len;
    }

    pub fn down(&mut self) {
        let len = self.shown().len().max(1);
        self.pick = (self.at() + 1) % len;
    }

    pub fn narrow(&mut self, filter: String) {
        self.filter = filter;
        self.pick = 0;
    }

    pub fn less(&mut self) {
        self.effort = self.effort.saturating_sub(1);
    }

    pub fn more(&mut self) {
        self.effort = (self.effort + 1).min(self.efforts.len() - 1);
    }

    pub fn effort(&self) -> Option<&str> {
        self.efforts[self.effort].as_deref()
    }

    /// The model and effort picked, when the filter leaves any model.
    pub fn chosen(&self) -> Option<(String, Option<String>)> {
        let model = self.shown().get(self.at())?.to_string();
        Some((model, self.effort().map(str::to_owned)))
    }
}

/// The provider of a model as the daemon names it, `provider/id`: the
/// binding in `config.toml` that serves it, as `bedrock` for
/// `bedrock/us.anthropic.claude-opus-5-5-v1:0`. None for a bare id.
pub fn provider(model: &str) -> Option<&str> {
    let (provider, _) = model.split_once('/')?;
    Some(provider).filter(|p| !p.is_empty())
}

/// A model and the provider that serves it, `Opus 5.5 · bedrock`, where
/// a view lists or explains its agents' models: `/status`, `/model`, the
/// welcome. One Sonny shows models by: the name as people say it
/// (`label`), the provider after it in lower case as `config.toml` names
/// it, and the raw id only where it is picked or typed. Where room is
/// tight (the footer, a commons post, the sky's key) the name stands
/// alone; a table gives the provider a column of its own, as `/usage`.
pub fn named(model: &str) -> String {
    match provider(model) {
        Some(provider) => format!("{} · {provider}", label(model)),
        None => label(model),
    }
}

/// A model as people say it, from the id a daemon or provider gives:
/// `bedrock/us.anthropic.claude-opus-5-5-v1:0` is `Opus 5.5` and
/// `openai/gpt-6.1-sol` is `GPT 6.1 Sol`, as Claude Code's and Codex's
/// pickers show names, not ids. The provider, a region and the vendor go,
/// with `claude-`, a date, a version suffix (`-v1:0`) and a window
/// (`[1m]`); version numbers join with dots and words take capitals.
pub fn label(model: &str) -> String {
    const PREFIXES: [&str; 17] = [
        "us",
        "eu",
        "apac",
        "global",
        "jp",
        "au",
        "ca",
        "anthropic",
        "openai",
        "meta",
        "amazon",
        "mistral",
        "cohere",
        "ai21",
        "deepseek",
        "qwen",
        "google",
    ];
    let mut id = model.rsplit_once('/').map_or(model, |(_, id)| id);
    id = id.split('[').next().unwrap_or(id);
    while let Some((first, rest)) = id.split_once('.')
        && PREFIXES.contains(&first)
    {
        id = rest;
    }
    // A Bedrock version, `-v1:0` or `-1:0`.
    if let Some((rest, version)) = id.rsplit_once('-')
        && version.contains(':')
    {
        id = rest;
    }
    let claude = id.strip_prefix("claude-");
    let mut words: Vec<&str> = claude
        .unwrap_or(id)
        .split('-')
        .filter(|w| !w.is_empty())
        .collect();
    // A date: `20250929`, or `2025-09-29`; or `latest`.
    let digits = |w: &str, n: usize| w.len() == n && w.bytes().all(|b| b.is_ascii_digit());
    if words.last().is_some_and(|w| digits(w, 8) || *w == "latest") {
        words.pop();
    }
    if let [.., y, m, d] = words[..]
        && digits(y, 4)
        && digits(m, 2)
        && digits(d, 2)
    {
        words.truncate(words.len() - 3);
    }
    let mut out: Vec<String> = Vec::new();
    let mut joining = false;
    for word in words {
        let number = (1..=2).contains(&word.len()) && word.bytes().all(|b| b.is_ascii_digit());
        if number
            && joining
            && let Some(last) = out.last_mut()
        {
            last.push('.');
            last.push_str(word);
            continue;
        }
        joining = number;
        out.push(match word {
            "gpt" | "oss" => word.to_uppercase(),
            // `o3`, `4o`, `120b` stay as they are written.
            _ if word.bytes().any(|b| b.is_ascii_digit()) => word.to_owned(),
            _ => {
                let mut chars = word.chars();
                chars.next().map_or_else(String::new, |first| {
                    first.to_uppercase().chain(chars).collect()
                })
            }
        });
    }
    // An old Claude id names its version before its kind.
    if claude.is_some()
        && out
            .first()
            .is_some_and(|w| w.starts_with(|c: char| c.is_ascii_digit()))
    {
        out.insert(0, "Claude".into());
    }
    match out.is_empty() {
        true => model.to_owned(),
        false => out.join(" "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_model_is_named_as_people_say_it() {
        for (id, said) in [
            ("bedrock/us.anthropic.claude-opus-5-5-v1:0", "Opus 5.5"),
            ("anthropic/claude-opus-5-5", "Opus 5.5"),
            ("claude-opus-4-8[1m]", "Opus 4.8"),
            (
                "bedrock/global.anthropic.claude-sonnet-4-5-20250929-v1:0",
                "Sonnet 4.5",
            ),
            ("anthropic/claude-3-5-sonnet-20241022", "Claude 3.5 Sonnet"),
            ("openai/gpt-6.1-sol", "GPT 6.1 Sol"),
            ("chatgpt/gpt-5.1-codex-max", "GPT 5.1 Codex Max"),
            ("bedrock/openai.gpt-oss-120b-1:0", "GPT OSS 120b"),
            ("openai/o3", "o3"),
            ("gemini-2.5-pro", "Gemini 2.5 Pro"),
            ("demo/scripted", "Scripted"),
            ("gpt-4o-2024-08-06", "GPT 4o"),
        ] {
            assert_eq!(label(id), said, "{id}");
        }
    }

    #[test]
    fn a_model_is_named_with_the_provider_that_serves_it() {
        let id = "bedrock/us.anthropic.claude-opus-5-5-v1:0";
        assert_eq!(provider(id), Some("bedrock"));
        assert_eq!(named(id), "Opus 5.5 · bedrock");
        assert_eq!(named("chatgpt/gpt-6.1-sol"), "GPT 6.1 Sol · chatgpt");
        // A bare id has no provider to name.
        assert_eq!(provider("claude-opus-4-8[1m]"), None);
        assert_eq!(named("claude-opus-4-8[1m]"), "Opus 4.8");
        assert_eq!(provider("/gpt-6"), None);
    }
    use serde_json::json;

    #[test]
    fn the_family_is_offered_and_the_current_choice_picked() {
        let providers = json!({
            "openai": {"family": "responses", "efforts": ["low", "medium", "high", "xhigh"]},
            "chatgpt": {"family": "responses", "efforts": ["low", "medium", "high", "xhigh"]},
            "anthropic": {"family": "anthropic", "efforts": ["low", "max"]},
        });
        let listing = json!({"providers": {
            "openai": {"models": [{"id": "gpt-6"}, {"id": "gpt-6-luna"}]},
            "chatgpt": {"models": [{"id": "gpt-6-luna"}]},
            "anthropic": {"models": [{"id": "claude-opus-5-5"}]},
        }});
        let mut picker = Picker::new(&listing, &providers, "chatgpt/gpt-6-luna", Some("high"));
        assert_eq!(
            picker.shown(),
            ["chatgpt/gpt-6-luna", "openai/gpt-6", "openai/gpt-6-luna"]
        );
        assert_eq!(
            picker.chosen(),
            Some(("chatgpt/gpt-6-luna".into(), Some("high".into())))
        );
        picker.down();
        picker.more();
        assert_eq!(
            picker.chosen(),
            Some(("openai/gpt-6".into(), Some("xhigh".into())))
        );
        picker.more();
        assert_eq!(picker.effort(), Some("xhigh"));
        picker.narrow("LUNA".into());
        assert_eq!(picker.shown().len(), 2);
        assert_eq!(picker.at(), 0);
        // The letters in order find a model, as fzf's do.
        // The letters are marked in the name the row shows,
        // `GPT 6 Luna · chatgpt`.
        picker.narrow("g6l".into());
        assert_eq!(picker.shown(), ["chatgpt/gpt-6-luna", "openai/gpt-6-luna"]);
        assert_eq!(picker.found()[0].1, [0, 4, 6]);
        // The provider finds its models.
        picker.narrow("open".into());
        assert_eq!(picker.shown(), ["openai/gpt-6", "openai/gpt-6-luna"]);
        // So does an id, though none of the name is marked.
        picker.narrow("gpt-6-l".into());
        assert_eq!(picker.shown(), ["chatgpt/gpt-6-luna", "openai/gpt-6-luna"]);
        assert!(picker.found()[0].1.is_empty());
        picker.narrow("nothing".into());
        assert_eq!(picker.chosen(), None);
    }

    #[test]
    fn a_provider_that_would_not_list_still_offers_the_current_model() {
        let providers = json!({"anthropic": {"family": "anthropic", "efforts": ["low"]}});
        let listing = json!({"providers": {"anthropic": {"error": "unauthorized"}}});
        let mut picker = Picker::new(&listing, &providers, "anthropic/claude-opus-5-5", None);
        assert_eq!(picker.shown(), ["anthropic/claude-opus-5-5"]);
        assert_eq!(picker.refused, ["anthropic: unauthorized"]);
        assert_eq!(picker.effort(), None);
        picker.less();
        assert_eq!(picker.effort(), None);
    }
}
