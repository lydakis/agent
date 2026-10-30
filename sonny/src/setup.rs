//! The first run, and `sonny setup` after it. Sonny first looks at what
//! the machine already has (a provider's key, a ChatGPT sign-in, AWS
//! profiles), so most people answer nothing but the model, and that
//! with Enter: the list arrives ranked, its first row marked. With no
//! provider anywhere it asks for one before it starts the daemon, as
//! Codex and Claude Code ask for a sign-in before their first turn.
//! The answers are kept as Codex keeps them: settings in a commented
//! `~/.agent/config.toml`, and keys in `~/.agent/env`, which Sonny applies
//! to the daemon and everything else it starts, so no shell profile needs
//! them.
use crate::picker::{self, Picked, Row};
use agent_client::Client;
use ratatui::crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode},
};
use serde_json::{Value, json};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

/// Someone is there to answer.
pub fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// What this machine already has for reaching a model.
#[derive(Debug, Default, PartialEq)]
struct Found {
    /// `agent login chatgpt` has signed in.
    chatgpt: bool,
    /// Codex is signed in with ChatGPT here, so the person has a plan.
    codex_chatgpt: bool,
    aws: Vec<Profile>,
    /// The key variables set, in the shell or `~/.agent/env`.
    keys: Vec<&'static str>,
    /// The config's `provider`, which setup offers to keep.
    current: Option<String>,
}

#[derive(Debug, PartialEq)]
struct Profile {
    name: String,
    /// Its region and how it signs in, as far as its file says.
    detail: String,
}

/// What `home` holds: Sonny's own sign-in, Codex's (only whether there
/// is one; its tokens are never read out), and the AWS profiles of the
/// files the AWS CLI reads.
fn detect(home: &Path, var: &dyn Fn(&str) -> Option<String>) -> Found {
    let codex = var("CODEX_HOME").map_or_else(|| home.join(".codex"), PathBuf::from);
    let codex_chatgpt = std::fs::read_to_string(codex.join("auth.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .is_some_and(|auth| {
            auth["auth_mode"]
                .as_str()
                .is_some_and(|m| m.eq_ignore_ascii_case("chatgpt"))
                || auth["tokens"].is_object()
        });
    let chatgpt = var("AGENT_CHATGPT_LOGIN")
        .map_or_else(|| home.join(".agent/chatgpt.json"), PathBuf::from)
        .is_file();
    let config = var("AWS_CONFIG_FILE").map_or_else(|| home.join(".aws/config"), PathBuf::from);
    let credentials = var("AWS_SHARED_CREDENTIALS_FILE")
        .map_or_else(|| home.join(".aws/credentials"), PathBuf::from);
    let read = |path: &Path| std::fs::read_to_string(path).unwrap_or_default();
    Found {
        chatgpt,
        codex_chatgpt,
        aws: profiles(&read(&config), &read(&credentials), var("AWS_PROFILE")),
        keys: KEYS
            .iter()
            .map(|(_, _, key)| *key)
            .filter(|key| var(key).is_some())
            .collect(),
        current: None,
    }
}

/// The profiles of an AWS `config` and `credentials` file: the one
/// `current` names first, then `default`, then the rest by name.
fn profiles(config: &str, credentials: &str, current: Option<String>) -> Vec<Profile> {
    let mut found: std::collections::BTreeMap<String, (Option<String>, Option<String>)> =
        Default::default();
    for (text, prefixed) in [(config, true), (credentials, false)] {
        let mut section: Option<String> = None;
        for line in text.lines().map(str::trim) {
            if let Some(header) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                let header = header.trim();
                section = match (prefixed, header.strip_prefix("profile ")) {
                    (true, Some(name)) => Some(name.trim().to_owned()),
                    (_, None) if header == "default" || !prefixed => Some(header.to_owned()),
                    // `sso-session` and `services` sections are no profile.
                    _ => None,
                };
                if let Some(name) = &section {
                    found.entry(name.clone()).or_default();
                }
                continue;
            }
            let (Some(name), Some((key, value))) = (&section, line.split_once('=')) else {
                continue;
            };
            let entry = found.entry(name.clone()).or_default();
            match key.trim() {
                "region" => entry.0 = Some(value.trim().to_owned()),
                "sso_session" | "sso_start_url" => entry.1 = Some("SSO".into()),
                "credential_process" => entry.1 = Some("a credential process".into()),
                "role_arn" => entry.1 = Some("an assumed role".into()),
                "aws_access_key_id" if entry.1.is_none() => entry.1 = Some("access keys".into()),
                _ => {}
            }
        }
    }
    let rank = |name: &str| match (current.as_deref() == Some(name), name == "default") {
        (true, _) => 0,
        (_, true) => 1,
        _ => 2,
    };
    let mut profiles: Vec<Profile> = found
        .into_iter()
        .map(|(name, (region, signs_in))| Profile {
            detail: [region, signs_in.map(|how| format!("signs in with {how}"))]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · "),
            name,
        })
        .collect();
    profiles.sort_by_key(|profile| rank(&profile.name));
    profiles
}

/// The providers a key reaches: their name, their spec and their key.
const KEYS: [(&str, &str, &str); 3] = [
    ("Anthropic", "anthropic", "ANTHROPIC_API_KEY"),
    ("OpenAI", "openai", "OPENAI_API_KEY"),
    ("OpenRouter", "openrouter", "OPENROUTER_API_KEY"),
];

#[derive(Clone, PartialEq)]
enum Way {
    /// The config's provider as it is.
    Keep,
    ChatGpt,
    Bedrock,
    /// A key's provider, by its index in [`KEYS`].
    Key(usize),
}

/// The ways to reach a model, what Sonny found for each, and the one to
/// mark: what is signed in, else what is found, else a ChatGPT plan.
fn ways(found: &Found) -> (Vec<(Way, Row)>, usize) {
    let profiles = |names: Vec<&str>| match names.len() {
        0 => String::new(),
        1..=3 => names.join(", "),
        n => format!("{} and {} more", names[..3].join(", "), n - 3),
    };
    let chatgpt = match (found.chatgpt, found.codex_chatgpt) {
        (true, _) => Row::new("ChatGPT plan", "signed in already").badge("ready"),
        (_, true) => Row::new(
            "ChatGPT plan",
            "Codex uses one here; sign in once in your browser",
        )
        .badge("found"),
        _ => Row::new(
            "ChatGPT plan",
            "Go, Plus or Pro, signed in through your browser",
        ),
    };
    let bedrock = match found.aws.len() {
        0 => Row::new("Amazon Bedrock", "with your AWS credentials"),
        n => Row::new(
            "Amazon Bedrock",
            format!(
                "{n} AWS profile{}: {}",
                if n == 1 { "" } else { "s" },
                profiles(found.aws.iter().map(|p| p.name.as_str()).collect())
            ),
        )
        .badge("found"),
    };
    let mut ways = vec![(Way::ChatGpt, chatgpt), (Way::Bedrock, bedrock)];
    for (index, (name, _, key)) in KEYS.iter().enumerate() {
        let label = format!("{name} API key");
        ways.push((
            Way::Key(index),
            match found.keys.contains(key) {
                true => Row::new(label, format!("{key} is set")).badge("ready"),
                false => Row::new(label, "paste a key"),
            },
        ));
    }
    if let Some(current) = &found.current {
        ways.insert(
            0,
            (
                Way::Keep,
                Row::new(format!("Keep {current}"), "the config's provider").badge("current"),
            ),
        );
    }
    // What is found comes first; the rest keep their order.
    ways.sort_by_key(|(_, row)| match row.badge.as_deref() {
        Some("current") => 0,
        Some("ready") => 1,
        Some(_) => 2,
        None => 3,
    });
    (ways, 0)
}

/// Asks which provider the daemon runs and keeps the answer, signing in
/// to ChatGPT through `agent` at `bin` when that is the one.
pub fn provider(bin: &Path) -> Result<(), String> {
    let file = agent_client::env_file::path().ok_or("set HOME")?;
    let config = agent_client::config::path().ok_or("set HOME")?;
    let home = PathBuf::from(std::env::var_os("HOME").ok_or("set HOME")?);
    let mut found = detect(&home, &|name| agent_client::env_file::var(name));
    found.current = agent_client::config::load()?.provider;
    WELCOMED.store(true, std::sync::atomic::Ordering::Relaxed);
    println!("Welcome to Sonny. Your agents need a model to talk to.");
    println!(
        "{DIM}Settings go in {}, and keys in {}, which only you can read.{RESET}\n",
        crate::tilde(&config.display().to_string()),
        crate::tilde(&file.display().to_string())
    );
    let (ways, marked) = ways(&found);
    let rows: Vec<Row> = ways.iter().map(|(_, row)| row.clone()).collect();
    let Picked::Row(chosen) = picker::pick("Connect with", &rows, marked, false)? else {
        unreachable!("a pick without typing is a row");
    };
    // The provider to name in the config, and what goes in the env file.
    let (named, kept): (Option<&str>, Vec<(&str, String)>) = match ways[chosen].0 {
        Way::Keep => (None, Vec::new()),
        Way::ChatGpt if found.chatgpt => (Some("chatgpt"), Vec::new()),
        Way::ChatGpt => {
            let mut command = crate::agent_command(bin)?;
            let status = command
                .args(["login", "chatgpt", "--pretty"])
                .status()
                .map_err(|e| format!("{}: {e}", bin.display()))?;
            if !status.success() {
                return Err("signing in to ChatGPT did not finish; run sonny again".into());
            }
            (Some("chatgpt"), Vec::new())
        }
        Way::Key(index) if found.keys.contains(&KEYS[index].2) => (Some(KEYS[index].1), Vec::new()),
        Way::Key(index) => {
            let (name, spec, var) = KEYS[index];
            let key = loop {
                let key = secret(&format!("Paste your {name} API key (it is not shown): "))?;
                if !key.is_empty() {
                    break key;
                }
            };
            (Some(spec), vec![(var, key)])
        }
        Way::Bedrock => {
            let kept = match aws_profile(&found.aws)? {
                Some(profile) => vec![("AWS_PROFILE", profile)],
                None => Vec::new(),
            };
            (Some("bedrock"), kept)
        }
    };
    if !kept.is_empty() {
        let pairs: Vec<(&str, &str)> = kept.iter().map(|(k, v)| (*k, v.as_str())).collect();
        agent_client::env_file::set(&file, &pairs)?;
    }
    save(&config, named.map(|provider| ("provider", provider)))?;
    println!();
    Ok(())
}

/// The AWS profile Bedrock signs in with, picked from those found; none
/// for the credentials the environment gives (keys, SSO, an instance role).
fn aws_profile(found: &[Profile]) -> Result<Option<String>, String> {
    if found.is_empty() {
        println!(
            "{DIM}No AWS profiles in ~/.aws; Bedrock signs in with the environment's credentials.{RESET}"
        );
        return Ok(None);
    }
    let mut rows: Vec<Row> = found
        .iter()
        .map(|profile| Row::new(&profile.name, &profile.detail))
        .collect();
    rows.push(Row::new(
        "no profile",
        "the environment's credentials: keys, SSO or an instance role",
    ));
    match picker::pick("AWS profile", &rows, 0, false)? {
        Picked::Row(index) if index < found.len() => Ok(Some(found[index].name.clone())),
        _ => Ok(None),
    }
}

/// Asks for the default model of new projects from what the daemon's
/// providers list, ranked, and keeps it as the config's `model`.
pub async fn model(client: &Client) -> Result<String, String> {
    let file = agent_client::config::path().ok_or("set HOME")?;
    if !WELCOMED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        println!("Welcome to Sonny. One choice and it's set up.\n");
    }
    let model = loop {
        print!("{DIM}Listing your providers' models...{RESET}");
        let _ = std::io::stdout().flush();
        let listing = client
            .request("provider_models", json!({}))
            .await
            .map_err(|e| e.to_string())?;
        print!("\r\x1b[K");
        let (models, refused) = listed(&listing);
        for (provider, error) in &refused {
            println!("Could not list {provider}'s models: {error}");
            if let Some(hint) = hint(provider, error) {
                println!("  {hint}");
            }
        }
        if models.is_empty() {
            // A sign-in done in another terminal lists them next time.
            let typed = line("Press Enter to list again, or type a model as PROVIDER/MODEL: ")?;
            match whole(&typed) {
                true => break typed,
                false => continue,
            }
        }
        let models = ranked(models);
        // Asked again, the model chosen before is marked.
        let current = agent_client::config::read(&file)?.model;
        let marked = (models.iter())
            .position(|model| current.as_ref() == Some(&model.id))
            .unwrap_or(0);
        let rows: Vec<Row> = models
            .iter()
            .enumerate()
            .map(|(index, model)| {
                let row = Row::new(&model.id, model.detail());
                match (index == marked && current.is_some(), index) {
                    (true, _) => row.badge("current"),
                    (_, 0) => row.badge("recommended"),
                    _ => row,
                }
            })
            .collect();
        println!("{DIM}The model new projects start with; /model changes a lead's later.{RESET}");
        let typed = loop {
            match picker::pick("Default model", &rows, marked, true)? {
                Picked::Row(index) => break models[index].id.clone(),
                Picked::Typed(text) if whole(&text) => break text,
                Picked::Typed(text) => {
                    println!("{text} is not PROVIDER/MODEL, as anthropic/claude-opus-5-5 is.")
                }
            }
        };
        break typed;
    };
    save(&file, Some(("model", &model)))?;
    summary(&model, &file);
    Ok(model)
}

/// What a refusal to list asks of the person, where Sonny knows.
fn hint(provider: &str, error: &str) -> Option<String> {
    if error.starts_with("provider_aws_credentials") {
        let profile = agent_client::env_file::var("AWS_PROFILE");
        return Some(match profile {
            Some(profile) => format!("Sign in with `aws sso login --profile {profile}`."),
            None => "Sign in with `aws sso login`, or set AWS_PROFILE in ~/.agent/env.".into(),
        });
    }
    match error {
        "provider_http_401" | "provider_http_403" => Some(format!(
            "The {provider} key was refused; set a new one in ~/.agent/env or run `sonny setup`."
        )),
        _ if error.starts_with("provider_login") => {
            Some("Sign in again with `sonny agent login chatgpt`.".into())
        }
        _ => None,
    }
}

/// `PROVIDER/MODEL`, whole.
fn whole(text: &str) -> bool {
    text.split_once('/')
        .is_some_and(|(provider, model)| !provider.is_empty() && !model.is_empty())
        && !text.contains(char::is_whitespace)
}

/// What setup chose, and where to change it.
fn summary(model: &str, file: &Path) {
    let sonny = agent_client::config::read(file).map(|c| c.sonny).ok();
    let effort = |effort: Option<String>| effort.unwrap_or_else(|| "the model's own".into());
    let (lead, task) = sonny.map_or((None, None), |s| (s.lead_effort, s.task_effort));
    let file = crate::tilde(&file.display().to_string());
    println!("\n{BOLD}Sonny is set up.{RESET}");
    println!("  Model       {model}");
    println!(
        "  Effort      leads {}, tasks {}",
        effort(lead),
        effort(task)
    );
    println!(
        "  Approvals   {}: {}",
        crate::approvals::MODES[0].0,
        crate::approvals::MODES[0].1
    );
    println!("  Settings    {file}");
    for missing in missing_tools() {
        println!("  {ACCENT}Missing{RESET}     {missing}");
    }
    println!("{DIM}Run `sonny setup` to choose again.{RESET}\n");
    *JUST_SET.lock().unwrap_or_else(|e| e.into_inner()) = Some(format!(
        "set up · settings in {file} · `sonny setup` changes them"
    ));
}

/// The welcome is said, by the provider's question or the model's.
static WELCOMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

static JUST_SET: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// The line a window shows once setup has just run in it.
pub fn just_set() -> Option<String> {
    JUST_SET.lock().unwrap_or_else(|e| e.into_inner()).take()
}

/// What agents need that this machine lacks, and how to install it.
fn missing_tools() -> Vec<String> {
    let on_path = |name: &str| {
        std::env::var_os("PATH")
            .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(name).is_file()))
    };
    let install = |package: &str| {
        let manager = [
            ("apt-get", "sudo apt install"),
            ("dnf", "sudo dnf install"),
            ("pacman", "sudo pacman -S"),
            ("zypper", "sudo zypper install"),
            ("apk", "sudo apk add"),
            ("brew", "brew install"),
        ]
        .into_iter()
        .find(|(bin, _)| on_path(bin));
        match manager {
            Some((_, command)) => format!("`{command} {package}`"),
            None => format!("your package manager's {package}"),
        }
    };
    let mut missing = Vec::new();
    // The paths the daemon looks in for bubblewrap.
    let bwrap = [
        "/usr/bin/bwrap",
        "/bin/bwrap",
        "/usr/local/bin/bwrap",
        "/run/current-system/sw/bin/bwrap",
    ];
    if cfg!(target_os = "linux") && !bwrap.iter().any(|path| Path::new(path).is_file()) {
        missing.push(format!(
            "bubblewrap, which agents' sandboxes need: {}",
            install("bubblewrap")
        ));
    }
    if !on_path("rg") {
        missing.push(format!(
            "ripgrep, which agents search with: {}",
            install("ripgrep")
        ));
    }
    missing
}

/// `sonny setup`: the provider and the model again, whatever is set, with
/// the daemon of `store` started or reached on `socket` to list models.
pub async fn again(store: &Path, socket: &Path) -> Result<(), String> {
    if !interactive() {
        return Err("sonny setup asks its questions in a terminal".into());
    }
    let before = agent_client::config::load()?.provider;
    let bin = crate::agent_bin().ok_or("Agent's binary was not found; set AGENT_BIN")?;
    provider(&bin)?;
    let running = Client::connect(socket).await.is_ok();
    if running && agent_client::config::load()?.provider != before {
        println!(
            "{DIM}The running daemon keeps its providers until it restarts: `sonny agent stop` restarts it, ending agents' turns.{RESET}"
        );
    }
    if !running {
        crate::start_daemon(store, socket, false).map_err(|e| e.to_string())?;
    }
    let (client, _events) = Client::connect(socket).await.map_err(|e| e.to_string())?;
    model(&client).await.map(|_| ())
}

/// The first lines of a new `~/.agent/config.toml`, saying what each
/// setting does; setup writes its own values below them.
const TEMPLATE: &str = "\
# Sonny's and the agent daemon's settings. Keys go in ~/.agent/env.
# `sonny setup` asks for the provider and model again.
#
# model = \"PROVIDER/MODEL\"  the model a new agent runs; /model changes a lead's
# provider = \"chatgpt\"      the daemon's: chatgpt, bedrock, anthropic, openai,
#                           openrouter, or NAME=FAMILY,URL,KEY_ENV; unset, the
#                           providers whose keys are set
# web_search = \"disabled\"   new agents search and fetch the web unless this says so
#
# [sonny]
# lead_effort, task_effort   a new lead's and each new task's effort: low, medium,
#                            high, xhigh, or max on Claude; unset, the model's own
# chime = true               rings when work ends or waits
#
# [judge]
# model = \"PROVIDER/MODEL\"  who decides calls in auto mode; unset, the agent's model
# effort = \"low\"
#
# [sonny.mcp_servers.NAME]   an MCP server; `sonny mcp add` writes one
";

/// `chosen` into the config file at `file`, with the efforts a first run
/// suggests where the file names none; the person may edit them after. A
/// new file starts with [`TEMPLATE`].
fn save(file: &Path, chosen: Option<(&str, &str)>) -> Result<(), String> {
    if !file.exists() {
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(file, TEMPLATE).map_err(|e| format!("{}: {e}", file.display()))?;
    }
    let sonny = agent_client::config::read(file)?.sonny;
    let suggested = [
        ("sonny.lead_effort", "medium", sonny.lead_effort),
        ("sonny.task_effort", "high", sonny.task_effort),
    ];
    let pairs: Vec<(&str, &str)> = (chosen.into_iter())
        .chain(
            suggested
                .iter()
                .filter(|(.., set)| set.is_none())
                .map(|(k, v, _)| (*k, *v)),
        )
        .collect();
    match pairs.is_empty() {
        true => Ok(()),
        false => agent_client::config::set(file, &pairs),
    }
}

const ACCENT: &str = "\x1b[38;2;255;190;60m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// A model a provider listed.
#[derive(Debug, PartialEq)]
struct Listed {
    /// `provider/id`.
    id: String,
    name: Option<String>,
    context: Option<u64>,
}

impl Listed {
    fn detail(&self) -> String {
        let context = self.context.map(|tokens| match tokens {
            t if t >= 1_000_000 && t % 1_000_000 == 0 => format!("{}M context", t / 1_000_000),
            t if t >= 1_000_000 => format!("{:.1}M context", t as f64 / 1_000_000.0),
            t => format!("{}K context", t / 1_000),
        });
        [self.name.clone(), context]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

/// Each model the providers listed, newest first as the daemon sorts
/// them, and each provider that would not list with what it said.
fn listed(listing: &Value) -> (Vec<Listed>, Vec<(String, String)>) {
    let mut models = Vec::new();
    let mut refused = Vec::new();
    let empty = serde_json::Map::new();
    let providers = listing["providers"].as_object().unwrap_or(&empty);
    for (provider, answer) in providers {
        if let Some(error) = answer["error"].as_str() {
            refused.push((provider.clone(), error.to_owned()));
        }
        for model in answer["models"].as_array().into_iter().flatten() {
            if let Some(id) = model["id"].as_str() {
                models.push(Listed {
                    id: format!("{provider}/{id}"),
                    name: model["name"].as_str().map(str::to_owned),
                    context: model["context_tokens"].as_u64(),
                });
            }
        }
    }
    (models, refused)
}

/// Words in an id that mark a model no agent runs on.
const NOT_AGENTS: [&str; 12] = [
    "embed",
    "tts",
    "whisper",
    "dall-e",
    "image",
    "moderation",
    "transcribe",
    "audio",
    "realtime",
    "speech",
    "rerank",
    "search-preview",
];

/// The models an agent can run, in the order to offer them: a dated
/// snapshot goes when its undated name is listed too, and Bedrock's
/// global inference profiles, which serve from any region, come before
/// a region's.
fn ranked(models: Vec<Listed>) -> Vec<Listed> {
    let ids: std::collections::HashSet<String> = models.iter().map(|m| m.id.clone()).collect();
    let mut kept: Vec<Listed> = models
        .into_iter()
        .filter(|model| {
            let id = model.id.to_lowercase();
            !NOT_AGENTS.iter().any(|word| id.contains(word))
        })
        .filter(|model| undated(&model.id).is_none_or(|alias| !ids.contains(alias)))
        .collect();
    kept.sort_by_key(|model| {
        let (provider, id) = model.id.split_once('/').unwrap_or(("", &model.id));
        provider == "bedrock" && !id.starts_with("global.")
    });
    kept
}

/// The name of a dated snapshot without its date, as `gpt-x` of
/// `gpt-x-2026-05-01` or `claude-y` of `claude-y-20260501`.
fn undated(id: &str) -> Option<&str> {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if let Some((base, date)) = id.rsplit_once('-')
        && date.len() == 8
        && digits(date)
    {
        return Some(base);
    }
    let mut parts = id.rsplitn(4, '-');
    let (day, month, year, base) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    (year.len() == 4 && month.len() == 2 && day.len() == 2)
        .then_some(base)
        .filter(|_| digits(year) && digits(month) && digits(day))
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
            "anthropic": {"models": [
                {"id": "claude-a", "name": "Claude A", "context_tokens": 200000},
                {"id": "claude-b"},
            ]},
            "openai": {"models": [], "error": "provider_http_401"},
        }});
        let (models, refused) = listed(&listing);
        assert_eq!(models[0].id, "anthropic/claude-a");
        assert_eq!(models[0].detail(), "Claude A · 200K context");
        assert_eq!(models[1].detail(), "");
        assert_eq!(refused, vec![("openai".into(), "provider_http_401".into())]);
        assert!(hint("openai", "provider_http_401").is_some());
    }

    #[test]
    fn models_no_agent_runs_and_dated_twins_go_and_global_profiles_lead() {
        let model = |id: &str| Listed {
            id: id.into(),
            name: None,
            context: None,
        };
        let ranked: Vec<String> = ranked(vec![
            model("openai/gpt-x"),
            model("openai/gpt-x-2026-05-01"),
            model("openai/gpt-y-2026-04-01"),
            model("openai/text-embedding-3-large"),
            model("openai/gpt-x-realtime"),
            model("anthropic/claude-a-20260501"),
            model("anthropic/claude-a"),
            model("bedrock/us.anthropic.claude-a"),
            model("bedrock/global.anthropic.claude-a"),
        ])
        .into_iter()
        .map(|m| m.id)
        .collect();
        assert_eq!(
            ranked,
            [
                "openai/gpt-x",
                // No undated twin: kept.
                "openai/gpt-y-2026-04-01",
                "anthropic/claude-a",
                "bedrock/global.anthropic.claude-a",
                "bedrock/us.anthropic.claude-a",
            ]
        );
        assert_eq!(undated("gpt-x"), None);
        assert_eq!(undated("claude-3-5"), None);
    }

    #[test]
    fn aws_profiles_come_from_both_files_the_current_one_first() {
        let config = "[default]\nregion = us-east-1\n\n[profile dev]\nsso_session = corp\nregion = eu-west-1\n\n[sso-session corp]\nsso_start_url = https://example.awsapps.com/start\n";
        let credentials = "[default]\naws_access_key_id = AKIA\n\n[ci]\naws_access_key_id = AKIB\n";
        let found = profiles(config, credentials, Some("dev".into()));
        let names: Vec<&str> = found.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["dev", "default", "ci"]);
        assert_eq!(found[0].detail, "eu-west-1 · signs in with SSO");
        assert_eq!(found[1].detail, "us-east-1 · signs in with access keys");
    }

    #[test]
    fn what_is_found_is_offered_first() {
        let home = std::env::temp_dir().join(format!("sonny-setup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::create_dir_all(home.join(".aws")).unwrap();
        std::fs::write(home.join(".codex/auth.json"), r#"{"auth_mode":"chatgpt"}"#).unwrap();
        std::fs::write(home.join(".aws/config"), "[profile dev]\n").unwrap();
        let found = detect(&home, &|_| None);
        assert!(found.codex_chatgpt && !found.chatgpt);
        assert_eq!(found.aws.len(), 1);
        let (offered, marked) = ways(&found);
        assert_eq!(marked, 0);
        assert!(matches!(offered[0].0, Way::ChatGpt | Way::Bedrock));
        assert!(offered[..2].iter().all(|(_, row)| row.badge.is_some()));
        // Nothing found: ChatGPT first, nothing marked as found.
        let (offered, _) = ways(&Found::default());
        assert!(offered[0].0 == Way::ChatGpt && offered.iter().all(|(_, r)| r.badge.is_none()));
        // A key set is ready, and a provider already chosen is kept first.
        let found = Found {
            keys: vec!["OPENAI_API_KEY"],
            current: Some("openai".into()),
            ..Found::default()
        };
        let (offered, _) = ways(&found);
        assert!(offered[0].0 == Way::Keep);
        assert!(offered[1].0 == Way::Key(1));
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn a_new_config_starts_with_the_template_and_reads() {
        let dir = std::env::temp_dir().join(format!("sonny-setup-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("config.toml");
        save(&file, Some(("model", "openai/gpt-x"))).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.starts_with("# Sonny's and the agent daemon's settings."));
        let config = agent_client::config::read(&file).unwrap();
        assert_eq!(config.model.as_deref(), Some("openai/gpt-x"));
        assert_eq!(config.sonny.lead_effort.as_deref(), Some("medium"));
        assert_eq!(config.sonny.task_effort.as_deref(), Some("high"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
