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

/// A step of setup that an upgrade brings, offered once by the first bot
/// Sonny opens after it, unless the person has done it.
pub struct Step {
    /// Its name, kept in `~/.agent/sonny/offered` once offered.
    pub id: &'static str,
    /// What it sets up, for the bot to offer.
    pub what: &'static str,
    /// The command that does it.
    pub how: &'static str,
    pub done: fn() -> bool,
}

/// Each step an upgrade can offer; first-run setup offers them itself.
pub const STEPS: [Step; 2] = [
    Step {
        id: "mcp-import",
        what: "the MCP servers the person's other agents (Claude Code, Codex, OpenCode, Kiro) reach, brought into Sonny",
        how: "sonny mcp import",
        // Done when none is left to bring; one Sonny cannot bring never counts.
        done: || {
            crate::mcp_import::here().map_or(true, |found| found.iter().all(|f| f.keys.is_empty()))
        },
    },
    Step {
        id: "backup",
        what: "memory, skills, profiles and config pushed every ten minutes to a private git \
               repository, so a lost machine loses little. Sonny names no host: find where \
               this machine keeps repositories (its checkouts' remotes, the tools signed in, \
               what the person's other agents made) and make a private one there",
        how: "sonny setup backup URL",
        done: || crate::home::Places::home().is_ok_and(|places| crate::home::done(&places)),
    },
];

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
    /// The other agents on `PATH`, by name.
    agents: Vec<&'static str>,
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
        agents: Vec::new(),
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
    /// Another agent on this machine, by its index in [`AGENTS`], set up
    /// with the person.
    Agent(usize),
}

/// The agents setup can hand itself to: their name, their command, and
/// how it starts a session there with a first message.
const AGENTS: [(&str, &str, &[&str]); 4] = [
    ("Claude Code", "claude", &[]),
    ("Codex", "codex", &[]),
    ("OpenCode", "opencode", &["--prompt"]),
    ("Kiro", "kiro-cli", &["chat"]),
];

/// The skill an agent setting Sonny up reads.
pub const SKILL: &str = include_str!("../skills/sonny-setup/SKILL.md");

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
    // With nothing ready, an agent this machine has can help, as it
    // reaches a model already.
    if !ways
        .iter()
        .any(|(_, row)| row.badge.as_deref() == Some("ready"))
    {
        for (index, (name, _, _)) in AGENTS.iter().enumerate() {
            if found.agents.contains(name) {
                ways.push((
                    Way::Agent(index),
                    Row::new(
                        format!("Let {name} set up Sonny"),
                        "it runs here with you, asking as it always does",
                    ),
                ));
            }
        }
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
/// to ChatGPT through `agent` at `bin` when that is the one. `again`, as
/// `sonny setup`, a key already set can be replaced.
pub fn provider(bin: &Path, again: bool) -> Result<(), String> {
    let file = agent_client::env_file::path().ok_or("set HOME")?;
    let config = agent_client::config::path().ok_or("set HOME")?;
    let home = PathBuf::from(std::env::var_os("HOME").ok_or("set HOME")?);
    let mut found = detect(&home, &|name| agent_client::env_file::var(name));
    found.current = agent_client::config::load()?.provider;
    found.agents = (AGENTS.iter())
        .filter(|(_, command, _)| on_path(command))
        .map(|(name, ..)| *name)
        .collect();
    WELCOMED.store(true, std::sync::atomic::Ordering::Relaxed);
    println!("Welcome to Sonny. Your agents need a model to talk to.");
    println!(
        "{DIM}Settings go in {}, and keys in {}, which only you can read.{RESET}\n",
        crate::tilde(&config.display().to_string()),
        crate::tilde(&file.display().to_string())
    );
    let (ways, marked) = ways(&found);
    let rows: Vec<Row> = ways.iter().map(|(_, row)| row.clone()).collect();
    let chosen = loop {
        let Picked::Row(chosen) = picker::pick("Connect with", &rows, marked, false)? else {
            unreachable!("a pick without typing is a row");
        };
        let Way::Agent(index) = ways[chosen].0 else {
            break chosen;
        };
        if hand_off(index)? {
            println!();
            return Ok(());
        }
    };
    // The provider to name in the config, and what goes in the env file.
    let (named, kept): (Option<&str>, Vec<(&str, String)>) = match ways[chosen].0 {
        Way::Keep => (None, Vec::new()),
        // Asked again, a sign-in that has expired can be made anew.
        Way::ChatGpt if found.chatgpt && !again => (Some("chatgpt"), Vec::new()),
        Way::ChatGpt
            if found.chatgpt
                && picker::pick(
                    "ChatGPT",
                    &[
                        Row::new("Keep the sign-in", "the one this machine has"),
                        Row::new("Sign in again", "in your browser, as the first time"),
                    ],
                    0,
                    false,
                )? == Picked::Row(0) =>
        {
            (Some("chatgpt"), Vec::new())
        }
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
        Way::Key(index) if found.keys.contains(&KEYS[index].2) => {
            let (name, spec, var) = KEYS[index];
            let key = match again {
                true => secret(&format!(
                    "Paste a new {name} API key, or press Enter to keep the one set: "
                ))?,
                false => String::new(),
            };
            match key.is_empty() {
                true => (Some(spec), Vec::new()),
                false => (Some(spec), vec![(var, key)]),
            }
        }
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
        Way::Agent(_) => unreachable!("an agent's setup returned above"),
        Way::Bedrock => {
            // No profile is written as an empty one, which unsets one
            // the shell or an earlier setup gave.
            let profile = aws_profile(&found.aws)?.unwrap_or_default();
            let kept = vec![("AWS_PROFILE", profile)];
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

/// Setup handed to the agent at `index` in [`AGENTS`], in a session of its
/// own in this terminal, with the person; whether a provider is set once
/// the person leaves it.
fn hand_off(index: usize) -> Result<bool, String> {
    let (name, command, args) = AGENTS[index];
    let skill = crate::skills::install("sonny-setup", SKILL)
        .or_else(|| {
            let home = std::env::var_os("HOME")?;
            Some(PathBuf::from(home).join(".agents/skills/sonny-setup/SKILL.md"))
        })
        .ok_or("set HOME")?;
    let sonny = std::env::current_exe().map_err(|e| e.to_string())?;
    let prompt = format!(
        "Set up Sonny, the coding-agent client on this machine, so its agents reach a model. \
         Follow the skill at {}. Sonny is {}; start with `{} setup --json` to see what this \
         machine has. Ask me before anything that needs my key or a sign-in. Once \
         `setup --json` shows a provider, tell me to leave this session and Sonny goes on.",
        skill.display(),
        sonny.display(),
        sonny.display()
    );
    println!("{DIM}Starting {name}. Leave it when Sonny is set up, and Sonny goes on.{RESET}\n");
    let status = std::process::Command::new(command)
        .args(args)
        .arg(&prompt)
        .status()
        .map_err(|e| format!("{command}: {e}"))?;
    let set = agent_client::config::load()?.provider.is_some();
    if !set {
        println!(
            "\n{name} {} and no provider is set yet. Choose one here.\n",
            match status.success() {
                true => "ended",
                false => "stopped",
            }
        );
    }
    Ok(set)
}

pub const USAGE: &str = "\
usage: sonny setup [--provider chatgpt|anthropic|openai|openrouter|bedrock]
                   [--profile AWS_PROFILE] [--key-stdin] [--model PROVIDER/MODEL]
                   [--import-mcp [codex,claude,opencode,kiro]] [--json]
With no flags, asks in a terminal. With flags, sets what they name and asks
nothing, for scripts and agents: --key-stdin reads the provider's key from
stdin into ~/.agent/env; --import-mcp brings the MCP servers of those agents
(all, when it names none) but those from files in this folder; --json prints
what this machine has and what is set, after any change.";

/// `sonny setup` with flags: what they name set, nothing asked.
pub async fn flags(args: &[String], socket: &Path) -> Result<(), String> {
    let mut provider = None;
    let mut profile = None;
    let mut key_stdin = false;
    let mut model = None;
    let mut import = None;
    let mut json = false;
    let mut args = args.iter().peekable();
    while let Some(arg) = args.next() {
        let mut value = |flag: &str| {
            args.next()
                .cloned()
                .ok_or_else(|| format!("{flag} takes a value\n{USAGE}"))
        };
        match arg.as_str() {
            "--provider" => provider = Some(value("--provider")?),
            "--profile" => profile = Some(value("--profile")?),
            "--model" => model = Some(value("--model")?),
            "--key-stdin" => key_stdin = true,
            "--json" => json = true,
            "--import-mcp" => {
                let named = args.next_if(|next| !next.starts_with("--"));
                import = Some(named.cloned().unwrap_or_default());
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            other => return Err(format!("no flag {other}\n{USAGE}")),
        }
    }
    let config = agent_client::config::path().ok_or("set HOME")?;
    let env = agent_client::env_file::path().ok_or("set HOME")?;
    let home = PathBuf::from(std::env::var_os("HOME").ok_or("set HOME")?);
    let found = detect(&home, &|name| agent_client::env_file::var(name));
    let before = reached()?;
    if let Some(provider) = &provider {
        let mut kept: Vec<(&str, String)> = Vec::new();
        match provider.as_str() {
            "chatgpt" if !found.chatgpt => {
                return Err("ChatGPT is not signed in here: run `sonny agent login chatgpt`, then this again".into());
            }
            "chatgpt" => {}
            "bedrock" => match (&profile, found.aws.len()) {
                (Some(name), _) if !found.aws.iter().any(|p| &p.name == name) => {
                    return Err(format!(
                        "no AWS profile {name}; these are: {}",
                        found
                            .aws
                            .iter()
                            .map(|p| p.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                (Some(name), _) => kept.push(("AWS_PROFILE", name.clone())),
                // None to name: the environment's credentials.
                (None, 0) => kept.push(("AWS_PROFILE", String::new())),
                (None, _) => {
                    return Err(format!(
                        "name an AWS profile with --profile: {}",
                        found
                            .aws
                            .iter()
                            .map(|p| p.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
            },
            spec => {
                let Some((name, _, var)) = KEYS.iter().find(|(_, s, _)| *s == spec) else {
                    return Err(format!("no provider {spec}\n{USAGE}"));
                };
                if key_stdin {
                    // Typed or pasted at a terminal, the key is not shown.
                    let key = match std::io::stdin().is_terminal() {
                        // On stderr, so stdout keeps `--json`'s one value.
                        true => masked(&format!("{name} key: "), &mut std::io::stderr())?,
                        false => {
                            let mut key = String::new();
                            std::io::stdin()
                                .read_line(&mut key)
                                .map_err(|e| e.to_string())?;
                            key.trim().to_owned()
                        }
                    };
                    let key = key.as_str();
                    if key.is_empty() {
                        return Err(format!("no {name} key on stdin"));
                    }
                    kept.push((var, key.to_owned()));
                } else if !found.keys.contains(var) {
                    return Err(format!(
                        "{var} is not set: set it in ~/.agent/env, or pass the key on stdin with --key-stdin"
                    ));
                }
            }
        }
        if profile.is_some() && provider != "bedrock" {
            return Err("--profile is Bedrock's".into());
        }
        if !kept.is_empty() {
            let pairs: Vec<(&str, &str)> = kept.iter().map(|(k, v)| (*k, v.as_str())).collect();
            agent_client::env_file::set(&env, &pairs)?;
        }
        save(&config, Some(("provider", provider)))?;
        // Another provider's model would not start; without `--model`,
        // the next new project asks for one.
        let other = (agent_client::config::read(&config)?.model)
            .is_some_and(|set| set.split_once('/').map(|(p, _)| p) != Some(provider.as_str()));
        if model.is_none() && other {
            agent_client::config::write(&config, &[("model", None)])?;
        }
    } else if profile.is_some() || key_stdin {
        return Err(format!(
            "--profile and --key-stdin go with --provider\n{USAGE}"
        ));
    }
    if let Some(model) = &model {
        if !whole(model) {
            return Err(format!(
                "{model} is not PROVIDER/MODEL, as anthropic/claude-opus-5-5 is"
            ));
        }
        save(&config, Some(("model", model)))?;
    }
    let brought = match &import {
        Some(agents) => {
            let agents: Vec<&str> = agents.split(',').filter(|a| !a.is_empty()).collect();
            crate::skills::note_offered("mcp-import");
            // `--json` keeps stdout for its one value.
            let say = |line: &str| match json {
                true => eprintln!("{line}"),
                false => println!("{line}"),
            };
            crate::mcp_import::offer_from(true, &agents, &say)?
        }
        None => Vec::new(),
    };
    if reached()? != before && Client::connect(socket).await.is_ok() {
        let notice = "The running daemon keeps the provider and keys it started with until it restarts: `sonny agent shutdown` stops it once agents' turns end.";
        match json {
            true => eprintln!("{notice}"),
            false => println!("{notice}"),
        }
    }
    match json {
        true => println!("{}", state(&home)?),
        false => {
            let changed = [
                provider.map(|p| format!("provider {p}")),
                model.map(|m| format!("model {m}")),
                (!brought.is_empty()).then(|| format!("MCP {}", brought.join(", "))),
            ];
            let changed: Vec<String> = changed.into_iter().flatten().collect();
            match changed.is_empty() {
                true => println!("{USAGE}"),
                false => println!(
                    "Set {}. Settings in {}, keys in {}.",
                    changed.join(", "),
                    crate::tilde(&config.display().to_string()),
                    crate::tilde(&env.display().to_string())
                ),
            }
        }
    }
    Ok(())
}

/// What `sonny setup --json` prints: what this machine has for reaching a
/// model, what is set, and what setup has left.
fn state(home: &Path) -> Result<Value, String> {
    let found = detect(home, &|name| agent_client::env_file::var(name));
    let config = agent_client::config::load()?;
    let mut ways = vec![
        json!({"provider": "chatgpt", "signed_in": found.chatgpt, "codex_signed_in": found.codex_chatgpt}),
        json!({"provider": "bedrock", "profiles": found.aws.iter()
            .map(|p| json!({"name": p.name, "detail": p.detail})).collect::<Vec<_>>()}),
    ];
    ways.extend(KEYS.iter().map(
        |(_, spec, var)| json!({"provider": spec, "key": var, "set": found.keys.contains(var)}),
    ));
    let mcp = crate::mcp_import::here().unwrap_or_default();
    Ok(json!({
        "config": agent_client::config::path(),
        "env": agent_client::env_file::path(),
        "provider": config.provider,
        "model": config.model,
        "ways": ways,
        "agents": AGENTS.iter().map(|(name, command, _)| {
            json!({"name": name, "command": command, "on_path": on_path(command)})
        }).collect::<Vec<_>>(),
        "mcp_to_import": crate::mcp_import::json(&mcp),
        "missing": missing_tools(),
        "steps": STEPS.iter().map(|step| {
            json!({"id": step.id, "what": step.what, "how": step.how, "done": (step.done)()})
        }).collect::<Vec<_>>(),
    }))
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
        println!("Welcome to Sonny. A question or two and it's set up.\n");
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
    // Servers the person's other agents reach, offered once a model can
    // use them; a refusal leaves setup done.
    crate::skills::note_offered("mcp-import");
    let brought = crate::mcp_import::offer(false).unwrap_or_else(|error| {
        println!("{DIM}MCP servers not brought in: {error}{RESET}");
        Vec::new()
    });
    let backed = backup().unwrap_or_else(|error| {
        if error != "setup cancelled" {
            println!("{DIM}Backup not set up: {error}{RESET}");
        }
        false
    });
    if let Err(error) = chrome()
        && error != "setup cancelled"
    {
        println!("{DIM}Chrome not downloaded: {error}{RESET}");
    }
    summary(&model, &file, &brought, backed);
    Ok(model)
}

/// Backup, offered once and last; an empty answer skips it.
fn backup() -> Result<bool, String> {
    let places = crate::home::Places::home()?;
    if crate::home::done(&places) || crate::skills::was_offered("backup") {
        return Ok(false);
    }
    crate::skills::note_offered("backup");
    println!(
        "\nBack up memory, skills, profiles and config.toml to a private git repository,\nso a new machine starts where this one left off. Keys in ~/.agent/env, logins\nand chat history stay on this machine."
    );
    let url = line("Repository URL, or Enter to skip (`sonny setup backup` later): ")?;
    if url.is_empty() {
        return Ok(false);
    }
    let file = agent_client::config::path().ok_or("set HOME")?;
    let chosen = agent_client::config::read(&file)?;
    let done = crate::home::set_up(&places, Some(&url));
    // A backup restored over this config brings its MCP servers and
    // efforts; the provider and model just chosen, whose keys are here, stay.
    let now = agent_client::config::read(&file)?;
    for (key, was, is) in [
        ("provider", chosen.provider, now.provider),
        ("model", chosen.model, now.model),
    ] {
        if let Some(was) = was.filter(|was| Some(was) != is.as_ref()) {
            save(&file, Some((key, &was)))?;
            println!("{DIM}Kept {key} {was}, chosen here.{RESET}");
        }
    }
    done.map(|()| true)
}

/// A Chrome for agent-browser to drive, offered when it finds none; an
/// answer other than yes skips it.
fn chrome() -> Result<(), String> {
    if has_chrome() != Some(false) {
        return Ok(());
    }
    println!(
        "\nAgents browse with agent-browser, which finds no Chrome here. It can download\nChrome for Testing for them, kept apart from any browser of yours."
    );
    let answer = line("Download it now? [Y/n] ")?;
    if !matches!(answer.to_ascii_lowercase().as_str(), "" | "y" | "yes") {
        return Ok(());
    }
    let ran = std::process::Command::new("agent-browser")
        .arg("install")
        .status()
        .map_err(|e| e.to_string())?;
    match ran.success() {
        true => Ok(()),
        false => Err(format!("`agent-browser install` {ran}")),
    }
}

/// Whether agent-browser finds a Chrome, by its own check; `None` when it
/// is not installed or does not say.
fn has_chrome() -> Option<bool> {
    if !on_path("agent-browser") {
        return None;
    }
    let report = std::process::Command::new("agent-browser")
        .args(["doctor", "--offline", "--quick", "--json"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    chrome_found(&serde_json::from_slice(&report.stdout).ok()?)
}

/// What `agent-browser doctor --json` says of Chrome.
fn chrome_found(report: &Value) -> Option<bool> {
    let check = report["checks"]
        .as_array()?
        .iter()
        .find(|check| check["id"] == "chrome.installed")?;
    Some(check["status"] == "pass")
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
fn summary(model: &str, file: &Path, brought: &[String], backed: bool) {
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
    if let Some(row) = crate::approvals::find(crate::approvals::default()) {
        println!("  Permissions {}: {}", row.label, row.about);
    }
    if !brought.is_empty() {
        println!("  MCP         {}", brought.join(", "));
    }
    if backed {
        println!(
            "  Backup      ~/agent-home, every {} minutes",
            crate::home::EVERY_SECONDS / 60
        );
    }
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

/// Whether `name` is a file in a `PATH` folder.
pub fn on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(name).is_file()))
}

/// What agents need that this machine lacks, and how to install it.
fn missing_tools() -> Vec<String> {
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
    // Not in distributions' packages: Homebrew's, or npm's.
    if !on_path("agent-browser") {
        let command = match on_path("brew") {
            true => "brew install agent-browser",
            false => "npm install -g agent-browser",
        };
        missing.push(format!(
            "agent-browser, which agents browse with: `{command}`"
        ));
    } else if has_chrome() == Some(false) {
        missing.push("Chrome, which agent-browser drives: `agent-browser install`".into());
    }
    missing
}

/// `sonny setup`: the provider and the model again, whatever is set, with
/// the daemon of `store` started or reached on `socket` to list models.
pub async fn again(store: &Path, socket: &Path) -> Result<(), String> {
    if !interactive() {
        return Err("sonny setup asks its questions in a terminal".into());
    }
    let before = reached()?;
    let bin = crate::agent_bin().ok_or("Agent's binary was not found; set AGENT_BIN")?;
    provider(&bin, true)?;
    let after = reached()?;
    if after != before && Client::connect(socket).await.is_ok() {
        // A running daemon keeps the provider and keys it started with, so
        // its listing would be of those. A model of another provider is
        // dropped, and the next new project asks again.
        let then = match after.provider != before.provider {
            true => {
                let file = agent_client::config::path().ok_or("set HOME")?;
                agent_client::config::write(&file, &[("model", None)])?;
                "Then run `sonny setup` again, or open a new project, to choose the model."
            }
            false => "It uses what you chose from then on.",
        };
        println!(
            "Saved. The running daemon keeps the provider and keys it started with until it restarts: `sonny agent shutdown` stops it once agents' turns end, and Sonny starts it again. {then}"
        );
        crate::skills::note_offered("mcp-import");
        if let Err(error) = crate::mcp_import::offer(false) {
            println!("{DIM}MCP servers not brought in: {error}{RESET}");
        }
        return Ok(());
    }
    if Client::connect(socket).await.is_err() {
        crate::start_daemon(store, socket, false).map_err(|e| e.to_string())?;
    }
    let (client, _events) = Client::connect(socket).await.map_err(|e| e.to_string())?;
    model(&client).await.map(|_| ())
}

/// What decides how the daemon reaches a model, as setup leaves it.
#[derive(PartialEq)]
struct Reached {
    provider: Option<String>,
    /// `~/.agent/env`, whole.
    env: Vec<u8>,
    /// The ChatGPT sign-in, whole.
    login: Vec<u8>,
}

fn reached() -> Result<Reached, String> {
    let read = |path: Option<PathBuf>| path.and_then(|p| std::fs::read(p).ok()).unwrap_or_default();
    let login = agent_client::env_file::var("AGENT_CHATGPT_LOGIN")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".agent/chatgpt.json")));
    Ok(Reached {
        provider: agent_client::config::load()?.provider,
        env: read(agent_client::env_file::path()),
        login: read(login),
    })
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
            // A fine-tuned model, `ft:BASE:org:suffix:id`, is its base's
            // kind, whatever its suffix says, as the daemon judges it.
            let id = model.id.to_lowercase();
            let id = id.split_once('/').map_or(id.as_str(), |(_, id)| id);
            let base = id
                .strip_prefix("ft:")
                .map_or(id, |ft| ft.split(':').next().unwrap_or(ft));
            !NOT_AGENTS.iter().any(|word| base.contains(word))
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
pub(crate) fn line(prompt: &str) -> Result<String, String> {
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
    masked(prompt, &mut std::io::stdout())
}

/// [`secret`], with the prompt and its line's end written to `out`.
fn masked(prompt: &str, out: &mut dyn Write) -> Result<String, String> {
    let _ = write!(out, "{prompt}");
    let _ = out.flush();
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
    let _ = writeln!(out);
    read.map(|()| typed.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_browser_s_doctor_says_whether_it_finds_chrome() {
        let report = |status: &str| {
            json!({"checks": [
                {"id": "env.version", "status": "pass"},
                {"id": "chrome.installed", "status": status},
            ]})
        };
        assert_eq!(chrome_found(&report("pass")), Some(true));
        assert_eq!(chrome_found(&report("fail")), Some(false));
        assert_eq!(chrome_found(&json!({"checks": []})), None);
    }

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
            model("openai/ft:gpt-x:org:image-tags:abc"),
            model("openai/ft:gpt-image-2:org:mine:abc"),
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
                // Judged by its base, not its suffix.
                "openai/ft:gpt-x:org:image-tags:abc",
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
        assert!(!offered.iter().any(|(way, _)| matches!(way, Way::Agent(_))));
        // Nothing ready: an agent on this machine can set it up, last.
        let found = Found {
            agents: vec!["Codex"],
            ..Found::default()
        };
        let (offered, _) = ways(&found);
        let (way, row) = offered.last().unwrap();
        assert!(*way == Way::Agent(1) && row.label == "Let Codex set up Sonny");
        // Something ready: no agent is offered.
        let found = Found {
            agents: vec!["Codex"],
            keys: vec!["OPENAI_API_KEY"],
            ..Found::default()
        };
        assert!(
            !ways(&found)
                .0
                .iter()
                .any(|(way, _)| matches!(way, Way::Agent(_)))
        );
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
