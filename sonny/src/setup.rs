//! The first run. With no provider anywhere, Sonny asks which one to use
//! before it starts the daemon, as Codex and Claude Code ask for a sign-in
//! before their first turn; with no model, it asks for the default one.
//! Both answers go to `~/.agent/env`, which Sonny applies to the daemon and
//! everything else it starts, so no shell profile needs them.
use agent_client::Client;
use ratatui::crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode},
};
use serde_json::{Value, json};
use std::io::{IsTerminal, Write};

/// Someone is there to answer.
pub fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

const PROVIDERS: [&str; 5] = [
    "ChatGPT plan (Go, Plus or Pro), signed in through your browser",
    "Anthropic API key",
    "OpenAI API key",
    "OpenRouter API key",
    "Amazon Bedrock, with your AWS credentials",
];

/// Asks which provider the daemon runs and keeps the answer, signing in
/// to ChatGPT through `agent` at `bin` when that is the one.
pub fn provider(bin: &std::path::Path) -> Result<(), String> {
    let file = agent_client::env_file::path().ok_or("set HOME")?;
    println!("Welcome to Sonny. Choose how your agents reach a model:\n");
    for (index, provider) in PROVIDERS.iter().enumerate() {
        println!("  {}. {provider}", index + 1);
    }
    println!(
        "\nYour choice is kept in {}, which only you can read.",
        crate::tilde(&file.display().to_string())
    );
    let choice = loop {
        let answer = line(&format!("Choice (1-{}): ", PROVIDERS.len()))?;
        match answer.parse::<usize>() {
            Ok(n) if (1..=PROVIDERS.len()).contains(&n) => break n,
            _ => continue,
        }
    };
    let kept: Vec<(&str, String)> = match choice {
        1 => {
            let mut command = crate::agent_command(bin)?;
            let status = command
                .args(["login", "chatgpt", "--pretty"])
                .status()
                .map_err(|e| format!("{}: {e}", bin.display()))?;
            if !status.success() {
                return Err("signing in to ChatGPT did not finish; run sonny again".into());
            }
            vec![("AGENT_PROVIDER", "chatgpt".into())]
        }
        2..=4 => {
            let (name, var) = [
                ("Anthropic", "ANTHROPIC_API_KEY"),
                ("OpenAI", "OPENAI_API_KEY"),
                ("OpenRouter", "OPENROUTER_API_KEY"),
            ][choice - 2];
            let key = loop {
                let key = secret(&format!("Paste your {name} API key (it is not shown): "))?;
                if !key.is_empty() {
                    break key;
                }
            };
            vec![(var, key)]
        }
        _ => {
            let current = std::env::var("AWS_PROFILE").unwrap_or_default();
            let asked = match current.is_empty() {
                true => "AWS profile (Enter for your default credentials): ".to_owned(),
                false => format!("AWS profile (Enter for {current}): "),
            };
            let profile = Some(line(&asked)?)
                .filter(|p| !p.is_empty())
                .unwrap_or(current);
            let mut kept = vec![("AGENT_PROVIDER", "bedrock".to_owned())];
            if !profile.is_empty() {
                kept.push(("AWS_PROFILE", profile));
            }
            kept
        }
    };
    let pairs: Vec<(&str, &str)> = kept.iter().map(|(k, v)| (*k, v.as_str())).collect();
    agent_client::env_file::set(&file, &pairs)?;
    println!("Saved.\n");
    Ok(())
}

/// Asks for the default model of new projects from what the daemon's
/// providers list, and keeps it as `AGENT_MODEL`.
pub async fn model(client: &Client) -> Result<String, String> {
    let file = agent_client::env_file::path().ok_or("set HOME")?;
    print!("Listing your providers' models... ");
    let _ = std::io::stdout().flush();
    let listing = client
        .request("provider_models", json!({}))
        .await
        .map_err(|e| e.to_string())?;
    print!("\r\x1b[K");
    let (models, refused) = listed(&listing);
    for refusal in &refused {
        println!("Could not list {refusal}");
    }
    println!("Choose the default model for new projects. Type part of a name to narrow the list.");
    let mut filter = String::new();
    let model = loop {
        let shown = narrow(&models, &filter);
        for (index, (id, name)) in shown.iter().take(SHOWN).enumerate() {
            match name {
                Some(name) => println!("  {:>2}. {id}  {name}", index + 1),
                None => println!("  {:>2}. {id}", index + 1),
            }
        }
        if shown.len() > SHOWN {
            println!("      ... {} more", shown.len() - SHOWN);
        }
        if models.is_empty() {
            println!("  Nothing was listed; type the model as PROVIDER/MODEL.");
        }
        let asked = match shown.as_slice() {
            [(only, _)] => format!("Model (Enter for {only}): "),
            _ => "Model: ".to_owned(),
        };
        let answer = line(&asked)?;
        match choose(&answer, &shown, &models) {
            Some(model) => break model,
            None => filter = answer,
        }
    };
    agent_client::env_file::set(&file, &[("AGENT_MODEL", &model)])?;
    println!(
        "Saved {model} as the default model in {}.\n",
        crate::tilde(&file.display().to_string())
    );
    Ok(model)
}

/// Models listed at once; typing narrows the rest.
const SHOWN: usize = 20;

/// `provider/id` and its name for each model the providers listed, in
/// their order, and what each provider that would not list said.
fn listed(listing: &Value) -> (Vec<(String, Option<String>)>, Vec<String>) {
    let mut models = Vec::new();
    let mut refused = Vec::new();
    let empty = serde_json::Map::new();
    let providers = listing["providers"].as_object().unwrap_or(&empty);
    for (provider, answer) in providers {
        if let Some(error) = answer["error"].as_str() {
            refused.push(format!("{provider}'s models: {error}"));
        }
        for model in answer["models"].as_array().into_iter().flatten() {
            if let Some(id) = model["id"].as_str() {
                let name = model["name"].as_str().map(str::to_owned);
                models.push((format!("{provider}/{id}"), name));
            }
        }
    }
    (models, refused)
}

/// The models whose id or name holds `filter`, ignoring case.
fn narrow<'a>(
    models: &'a [(String, Option<String>)],
    filter: &str,
) -> Vec<&'a (String, Option<String>)> {
    let filter = filter.to_lowercase();
    models
        .iter()
        .filter(|(id, name)| {
            id.to_lowercase().contains(&filter)
                || name
                    .as_deref()
                    .is_some_and(|n| n.to_lowercase().contains(&filter))
        })
        .collect()
}

/// The model an answer picks: a number from the list `shown`, Enter for
/// the one model left, a listed `PROVIDER/MODEL` typed whole, or one that
/// matches nothing listed, which the daemon still runs. Anything else
/// narrows the list.
fn choose(
    answer: &str,
    shown: &[&(String, Option<String>)],
    models: &[(String, Option<String>)],
) -> Option<String> {
    if let Ok(n) = answer.parse::<usize>() {
        return (1..=shown.len().min(SHOWN))
            .contains(&n)
            .then(|| shown[n - 1].0.clone());
    }
    if answer.is_empty() {
        return match shown {
            [(only, _)] => Some(only.clone()),
            _ => None,
        };
    }
    let whole = answer
        .split_once('/')
        .is_some_and(|(provider, model)| !provider.is_empty() && !model.is_empty())
        && !answer.contains(char::is_whitespace);
    let listed = models.iter().any(|(id, _)| id == answer);
    (whole && (listed || narrow(models, answer).is_empty())).then(|| answer.to_owned())
}

/// One line typed after `prompt`, trimmed.
fn line(prompt: &str) -> Result<String, String> {
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    match std::io::stdin().read_line(&mut answer) {
        Ok(0) => Err("setup cancelled".into()),
        Ok(_) => Ok(answer.trim().to_owned()),
        Err(error) => Err(error.to_string()),
    }
}

/// A line typed after `prompt` without showing it, as a key is.
fn secret(prompt: &str) -> Result<String, String> {
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    enable_raw_mode().map_err(|e| e.to_string())?;
    let mut typed = String::new();
    let read = loop {
        match event::read() {
            Ok(Event::Key(key)) if key.kind != KeyEventKind::Release => match key.code {
                KeyCode::Enter => break Ok(()),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    break Err("setup cancelled".to_owned());
                }
                KeyCode::Char(c) => typed.push(c),
                KeyCode::Backspace => {
                    typed.pop();
                }
                _ => {}
            },
            Ok(Event::Paste(text)) => typed.push_str(&text),
            Ok(_) => {}
            Err(error) => break Err(error.to_string()),
        }
    };
    let _ = disable_raw_mode();
    println!();
    read.map(|()| typed.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_listing_reads_as_provider_models_and_refusals() {
        let listing = json!({"providers": {
            "anthropic": {"models": [{"id": "claude-a", "name": "Claude A"}, {"id": "claude-b"}]},
            "openai": {"models": [], "error": "provider_auth_failed"},
        }});
        let (models, refused) = listed(&listing);
        assert_eq!(
            models,
            vec![
                ("anthropic/claude-a".to_owned(), Some("Claude A".to_owned())),
                ("anthropic/claude-b".to_owned(), None),
            ]
        );
        assert_eq!(refused, vec!["openai's models: provider_auth_failed"]);
    }

    #[test]
    fn a_number_a_last_match_or_a_whole_name_picks_and_anything_else_narrows() {
        let models = vec![
            (
                "anthropic/claude-a".to_owned(),
                Some("Claude Opus".to_owned()),
            ),
            (
                "anthropic/claude-b".to_owned(),
                Some("Claude Haiku".to_owned()),
            ),
            ("openai/gpt-x".to_owned(), None),
        ];
        let all = narrow(&models, "");
        assert_eq!(
            choose("2", &all, &models).as_deref(),
            Some("anthropic/claude-b")
        );
        assert_eq!(choose("4", &all, &models), None);
        assert_eq!(choose("", &all, &models), None);
        assert_eq!(choose("claude", &all, &models), None);
        // Part of a name narrows, even with its slash.
        assert_eq!(choose("anthropic/claude", &all, &models), None);
        assert_eq!(narrow(&models, "anthropic/claude").len(), 2);
        let opus = narrow(&models, "OPUS");
        assert_eq!(opus.len(), 1);
        assert_eq!(
            choose("", &opus, &models).as_deref(),
            Some("anthropic/claude-a")
        );
        assert_eq!(
            choose("openai/gpt-x", &all, &models).as_deref(),
            Some("openai/gpt-x")
        );
        assert_eq!(
            choose("bedrock/unlisted", &all, &models).as_deref(),
            Some("bedrock/unlisted")
        );
    }
}
