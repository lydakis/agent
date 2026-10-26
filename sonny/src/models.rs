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

    /// The models the filter leaves, in order.
    pub fn shown(&self) -> Vec<&str> {
        let filter = self.filter.to_lowercase();
        self.models
            .iter()
            .filter(|m| m.to_lowercase().contains(&filter))
            .map(String::as_str)
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

#[cfg(test)]
mod tests {
    use super::*;
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
