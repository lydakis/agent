//! MCP servers set up in other agents, brought into Sonny's
//! `[sonny.mcp_servers]` as Claude Code brings Claude Desktop's in: Codex's
//! `~/.codex/config.toml`, Claude Code's `~/.claude.json` and a folder's
//! `.mcp.json`, OpenCode's `opencode.json` and Kiro's `.kiro/settings/mcp.json`,
//! each in the user's home and in the folder Sonny runs in. A secret
//! written into one of those files (a key in a server's environment, a
//! bearer token) goes to `~/.agent/env`, never into `config.toml`, and the
//! server names it there. Nothing in the other agents' files changes.
use crate::picker::{self, Row};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A server another agent has, as Sonny would keep it.
#[derive(Debug, PartialEq)]
pub struct Found {
    pub name: String,
    /// The agent it came from, as `Claude Code`.
    pub from: &'static str,
    /// Its keys under Sonny's names.
    pub keys: toml::Table,
    /// Secrets for `~/.agent/env`, by variable.
    pub secrets: Vec<(String, String)>,
    /// Variables it reads from the environment it is started in.
    pub needs: Vec<String>,
    /// What did not come across and why, for the person to set by hand.
    pub notes: Vec<String>,
    /// From a file in the folder, which whoever wrote the folder wrote: as
    /// Claude Code asks before it starts a project's servers, Sonny brings
    /// one only when the person ticks it.
    pub folder: bool,
}

/// A server as another agent's file describes it, before translating.
#[derive(Default)]
struct Raw {
    name: String,
    command: Option<String>,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: Option<String>,
    url: Option<String>,
    headers: BTreeMap<String, String>,
    enabled: bool,
    disabled_tools: Vec<String>,
    /// Keys Sonny keeps as they are, from Codex, whose names Sonny shares.
    kept: toml::Table,
    /// Why it cannot come across at all.
    refused: Option<String>,
    folder: bool,
}

/// The agents whose servers are offered, in the order a name two of them
/// share is taken from.
const AGENTS: [&str; 4] = ["Codex", "Claude Code", "OpenCode", "Kiro"];

/// Every server the other agents have in `home` and `folder` that Sonny's
/// config does not name yet, and the names skipped for that.
/// Only `from`'s agents are read when it names any, so an agent left out
/// never hides a name another brings.
pub fn found(
    home: &Path,
    folder: &Path,
    var: &dyn Fn(&str) -> Option<String>,
    from: &[&str],
) -> Vec<Found> {
    let mine = agent_client::config::load()
        .map(|config| config.sonny.mcp_servers)
        .unwrap_or_default();
    let mut found: Vec<Found> = Vec::new();
    for agent in AGENTS
        .iter()
        .filter(|agent| from.is_empty() || from.contains(agent))
    {
        for raw in read(agent, home, folder, var) {
            let found_now = translate(agent, raw);
            if mine.contains_key(&found_now.name) || found.iter().any(|f| f.name == found_now.name)
            {
                continue;
            }
            found.push(found_now);
        }
    }
    found
}

/// The servers `agent`'s files in `home` and `folder` describe.
fn read(agent: &str, home: &Path, folder: &Path, var: &dyn Fn(&str) -> Option<String>) -> Vec<Raw> {
    let json = |path: PathBuf| -> Option<Value> {
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&text)
            .ok()
            .or_else(|| serde_json::from_str(&uncommented(&text)).ok())
    };
    let mut raws = Vec::new();
    match agent {
        "Codex" => {
            let dir = var("CODEX_HOME").map_or_else(|| home.join(".codex"), PathBuf::from);
            let table = std::fs::read_to_string(dir.join("config.toml"))
                .ok()
                .and_then(|text| text.parse::<toml::Table>().ok());
            if let Some(servers) = table
                .as_ref()
                .and_then(|t| t.get("mcp_servers"))
                .and_then(toml::Value::as_table)
            {
                raws.extend(
                    servers
                        .iter()
                        .filter_map(|(name, server)| codex(name, server)),
                );
            }
        }
        "Claude Code" => {
            let user = json(home.join(".claude.json")).unwrap_or_default();
            let here = folder.to_string_lossy();
            let shared = json(folder.join(".mcp.json")).unwrap_or_default();
            for (servers, folder) in [
                (&user["mcpServers"], false),
                (&user["projects"][here.as_ref()]["mcpServers"], false),
                (&shared["mcpServers"], true),
            ] {
                raws.extend(
                    (servers.as_object().into_iter().flatten()).map(|(n, s)| Raw {
                        folder,
                        ..claude(n, s)
                    }),
                );
            }
        }
        "OpenCode" => {
            let global = var("XDG_CONFIG_HOME")
                .map_or_else(|| home.join(".config"), PathBuf::from)
                .join("opencode");
            let files = [
                (var("OPENCODE_CONFIG").map(PathBuf::from), false),
                (Some(global.join("opencode.json")), false),
                (Some(global.join("opencode.jsonc")), false),
                (Some(folder.join("opencode.json")), true),
                (Some(folder.join("opencode.jsonc")), true),
            ];
            for (file, folder) in files {
                let Some(config) = file.and_then(json) else {
                    continue;
                };
                raws.extend(
                    (config["mcp"].as_object().into_iter().flatten()).map(|(n, s)| Raw {
                        folder,
                        ..opencode(n, s)
                    }),
                );
            }
        }
        _ => {
            let global = var("KIRO_HOME").map_or_else(|| home.join(".kiro"), PathBuf::from);
            for (file, folder) in [(global, false), (folder.join(".kiro"), true)] {
                if let Some(config) = json(file.join("settings/mcp.json")) {
                    raws.extend(
                        (config["mcpServers"].as_object().into_iter().flatten()).map(|(n, s)| {
                            Raw {
                                folder,
                                ..claude(n, s)
                            }
                        }),
                    );
                }
            }
        }
    }
    raws
}

/// Codex's `[mcp_servers.NAME]`, whose keys are Sonny's own.
fn codex(name: &str, server: &toml::Value) -> Option<Raw> {
    let table = server.as_table()?;
    let mut raw = Raw {
        name: name.to_owned(),
        enabled: true,
        ..Raw::default()
    };
    for (key, value) in table {
        match key.as_str() {
            "env" => {
                for (k, v) in value.as_table().into_iter().flatten() {
                    if let Some(v) = v.as_str() {
                        raw.env.insert(k.clone(), v.to_owned());
                    }
                }
            }
            "env_vars" => {
                // Passed through from Codex's environment: Sonny's servers
                // inherit the tool host's, which ~/.agent/env sets.
                for var in value
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v.as_str())
                {
                    raw.env.insert(var.to_owned(), format!("${{{var}}}"));
                }
            }
            "env_http_headers" => {
                for (header, var) in value.as_table().into_iter().flatten() {
                    if let Some(var) = var.as_str() {
                        raw.headers.insert(header.clone(), format!("${{{var}}}"));
                    }
                }
            }
            // Literal headers go where the other agents' do, so a secret
            // in one moves out like theirs.
            "http_headers" => {
                for (header, value) in value.as_table().into_iter().flatten() {
                    if let Some(value) = value.as_str() {
                        raw.headers.insert(header.clone(), value.to_owned());
                    }
                }
            }
            "command"
            | "args"
            | "cwd"
            | "url"
            | "bearer_token_env_var"
            | "enabled"
            | "enabled_tools"
            | "disabled_tools"
            | "startup_timeout_sec"
            | "tool_timeout_sec"
            | "description" => {
                raw.kept.insert(key.clone(), value.clone());
            }
            _ => {}
        }
    }
    Some(raw)
}

/// Claude Code's `mcpServers` entry, which Kiro's shares.
fn claude(name: &str, server: &Value) -> Raw {
    let strings = |value: &Value| -> Vec<String> {
        (value.as_array().into_iter().flatten())
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect()
    };
    let pairs = |value: &Value| -> BTreeMap<String, String> {
        (value.as_object().into_iter().flatten())
            .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
            .collect()
    };
    let refused = match server["type"].as_str() {
        Some("sse") => Some("it speaks the old SSE transport; Sonny speaks streamable HTTP".into()),
        Some("stdio" | "http") | None => None,
        Some(other) => Some(format!("Sonny does not speak its {other} transport")),
    };
    Raw {
        name: name.to_owned(),
        command: server["command"].as_str().map(str::to_owned),
        args: strings(&server["args"]),
        env: pairs(&server["env"]),
        cwd: server["cwd"].as_str().map(str::to_owned),
        url: server["url"].as_str().map(str::to_owned),
        headers: pairs(&server["headers"]),
        enabled: server["disabled"] != true,
        disabled_tools: strings(&server["disabledTools"]),
        refused,
        ..Raw::default()
    }
}

/// OpenCode's `mcp` entry: `local` with its command as one list, or
/// `remote`.
fn opencode(name: &str, server: &Value) -> Raw {
    let pairs = |value: &Value| -> BTreeMap<String, String> {
        (value.as_object().into_iter().flatten())
            .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
            .collect()
    };
    let mut raw = Raw {
        name: name.to_owned(),
        enabled: server["enabled"] != false,
        cwd: server["cwd"].as_str().map(str::to_owned),
        ..Raw::default()
    };
    match server["type"].as_str() {
        Some("local") => {
            let mut command = (server["command"].as_array().into_iter().flatten())
                .filter_map(|v| v.as_str().map(str::to_owned));
            raw.command = command.next();
            raw.args = command.collect();
            raw.env = pairs(&server["environment"]);
        }
        Some("remote") => {
            raw.url = server["url"].as_str().map(str::to_owned);
            raw.headers = pairs(&server["headers"]);
        }
        other => raw.refused = Some(format!("its type {other:?} is not local or remote")),
    }
    raw
}

/// JSONC as JSON: `//` and `/* */` comments, then trailing commas,
/// outside strings dropped.
fn uncommented(text: &str) -> String {
    let mut bare = String::with_capacity(text.len());
    outside_strings(text, &mut bare, |c, chars, out| match (c, chars.peek()) {
        ('/', Some('/')) => {
            for c in chars.by_ref() {
                if c == '\n' {
                    out.push('\n');
                    break;
                }
            }
        }
        ('/', Some('*')) => {
            chars.next();
            let mut last = ' ';
            for c in chars.by_ref() {
                if last == '*' && c == '/' {
                    break;
                }
                last = c;
            }
        }
        _ => out.push(c),
    });
    let mut out = String::with_capacity(bare.len());
    outside_strings(&bare, &mut out, |c, chars, out| {
        let trailing =
            c == ',' && matches!(chars.clone().find(|c| !c.is_whitespace()), Some('}' | ']'));
        if !trailing {
            out.push(c);
        }
    });
    out
}

/// Copies `text` to `out`, handing each character outside a JSON string to
/// `each` with the characters after it.
fn outside_strings(
    text: &str,
    out: &mut String,
    mut each: impl FnMut(char, &mut std::iter::Peekable<std::str::Chars>, &mut String),
) {
    let mut chars = text.chars().peekable();
    let mut string = false;
    while let Some(c) = chars.next() {
        if string {
            out.push(c);
            match c {
                '\\' => out.extend(chars.next()),
                '"' => string = false,
                _ => {}
            }
        } else if c == '"' {
            string = true;
            out.push(c);
        } else {
            each(c, &mut chars, out);
        }
    }
}

/// The variable a value only names, in any of the agents' spellings:
/// `${VAR}` (Claude Code, Kiro), `{env:VAR}` (OpenCode), or a default
/// after `:-`, which Sonny cannot keep.
fn named(value: &str) -> Option<&str> {
    let inner = value
        .strip_prefix("${")
        .and_then(|v| v.strip_suffix('}'))
        .or_else(|| {
            value
                .strip_prefix("{env:")
                .and_then(|v| v.strip_suffix('}'))
        })?;
    let var = inner.split(":-").next()?;
    (!var.is_empty() && var.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')).then_some(var)
}

/// A variable name or header that holds a secret.
fn secret(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    [
        "TOKEN",
        "KEY",
        "SECRET",
        "PASSWORD",
        "PASS",
        "AUTH",
        "CREDENTIAL",
        "PAT",
        "COOKIE",
    ]
    .iter()
    .any(|word| {
        name.split(|c: char| !c.is_ascii_alphanumeric())
            .any(|part| part == *word || part.ends_with(word))
    })
}

/// `raw` under Sonny's names, its secrets moved out.
fn translate(from: &'static str, raw: Raw) -> Found {
    let name: String = raw
        .name
        .chars()
        .map(
            |c| match c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                true => c,
                false => '-',
            },
        )
        .take(64)
        .collect();
    let mut found = Found {
        name: name.clone(),
        from,
        keys: raw.kept,
        secrets: Vec::new(),
        needs: Vec::new(),
        notes: Vec::new(),
        folder: raw.folder,
    };
    if let Some(why) = raw.refused {
        found.notes.push(format!("not brought in: {why}"));
        found.keys.clear();
        return found;
    }
    if raw.command.is_none()
        && raw.url.is_none()
        && !found.keys.contains_key("command")
        && !found.keys.contains_key("url")
    {
        found
            .notes
            .push("not brought in: it names no command or URL".into());
        found.keys.clear();
        return found;
    }
    // Named for Sonny, so it never takes over a variable the person uses.
    let token_var = format!(
        "SONNY_MCP_{}_TOKEN",
        name.to_ascii_uppercase().replace('-', "_")
    );
    // Other agents fill in `${VAR}` in these; Sonny starts them as written.
    let filled = (raw.command.iter().chain(&raw.args).chain(&raw.url))
        .any(|text| text.contains("${") || text.contains("{env:"));
    if filled {
        found.notes.push(
            "its command, args or URL name a variable, which Sonny passes as written; edit them in config.toml".into(),
        );
    }
    if let Some(command) = raw.command {
        found.keys.insert("command".into(), command.into());
    }
    if !raw.args.is_empty() {
        found.keys.insert("args".into(), raw.args.into());
    }
    if let Some(cwd) = raw.cwd {
        found.keys.insert("cwd".into(), cwd.into());
    }
    if let Some(url) = raw.url {
        found.keys.insert("url".into(), url.into());
    }
    if !raw.enabled {
        found.keys.insert("enabled".into(), false.into());
    }
    if !raw.disabled_tools.is_empty() {
        found
            .keys
            .insert("disabled_tools".into(), raw.disabled_tools.into());
    }
    // A server's environment: a value naming its own variable is
    // inherited, a secret goes to ~/.agent/env under its own name (which
    // the server is started with), and the rest stays.
    let mut env = found
        .keys
        .remove("env")
        .and_then(|v| v.as_table().cloned())
        .unwrap_or_default();
    for (key, value) in raw.env {
        match named(&value) {
            Some(var) if var == key => found.needs.push(key),
            Some(var) => found
                .notes
                .push(format!("{key} came from {var}; set {key} in ~/.agent/env")),
            None if secret(&key) => found.secrets.push((key, value)),
            None => {
                env.insert(key, value.into());
            }
        }
    }
    if !env.is_empty() {
        found.keys.insert("env".into(), toml::Value::Table(env));
    }
    let mut headers = found
        .keys
        .remove("http_headers")
        .and_then(|v| v.as_table().cloned())
        .unwrap_or_default();
    for (header, value) in raw.headers {
        let bearer = header
            .eq_ignore_ascii_case("authorization")
            .then(|| value.strip_prefix("Bearer ").map(str::trim))
            .flatten();
        match (bearer, named(&value)) {
            (Some(token), _) => {
                let var = match named(token) {
                    Some(var) => {
                        found.needs.push(var.to_owned());
                        var.to_owned()
                    }
                    None => {
                        found.secrets.push((token_var.clone(), token.to_owned()));
                        token_var.clone()
                    }
                };
                found.keys.insert("bearer_token_env_var".into(), var.into());
            }
            (None, Some(var)) => found
                .notes
                .push(format!("header {header} came from {var}; Sonny sends no header from a variable but the bearer token")),
            (None, None) if secret(&header) => found
                .notes
                .push(format!("header {header} holds a secret; add it to http_headers by hand")),
            (None, None) => {
                headers.insert(header, value.into());
            }
        }
    }
    if !headers.is_empty() {
        found
            .keys
            .insert("http_headers".into(), toml::Value::Table(headers));
    }
    found
}

/// Each secret of `chosen` that the environment children get (`kept`, the
/// shell's with `~/.agent/env` over it) or a server before it already holds
/// with another value stays in its server's `env` instead, since one
/// variable can hold only one; a URL server's token takes a free name.
fn settle(chosen: &mut [Found], kept: &[(String, String)]) {
    let mut taken: BTreeMap<String, String> = kept.iter().cloned().collect();
    for found in chosen {
        let mut env = found
            .keys
            .remove("env")
            .and_then(|v| v.as_table().cloned())
            .unwrap_or_default();
        let bearer = found
            .keys
            .get("bearer_token_env_var")
            .and_then(|v| v.as_str());
        let bearer = bearer.map(str::to_owned);
        for (var, value) in &mut found.secrets {
            // A URL server takes no env, so its token gets a free name.
            if bearer.as_ref() != Some(var) {
                continue;
            }
            let base = var.clone();
            let mut n = 2;
            while taken.get(var.as_str()).is_some_and(|held| held != value) {
                *var = format!("{base}_{n}");
                n += 1;
            }
            found
                .keys
                .insert("bearer_token_env_var".into(), var.clone().into());
        }
        found.secrets.retain(|(var, value)| match taken.get(var) {
            Some(held) if held != value => {
                env.insert(var.clone(), value.clone().into());
                found.notes.push(format!(
                    "{var} stays in config.toml: ~/.agent/env holds another {var}"
                ));
                false
            }
            _ => {
                taken.insert(var.clone(), value.clone());
                true
            }
        });
        if !env.is_empty() {
            found.keys.insert("env".into(), toml::Value::Table(env));
        }
    }
}

/// Writes `chosen` to Sonny's config and their secrets to `~/.agent/env`.
pub fn bring(mut chosen: Vec<Found>) -> Result<Vec<Found>, String> {
    let config = agent_client::config::path().ok_or("set HOME")?;
    let env = agent_client::env_file::path().ok_or("set HOME")?;
    // What children see: the shell's environment, with the file over it.
    // A name the file unsets stays taken: the person meant it unset.
    let mut kept: BTreeMap<String, String> = std::env::vars().collect();
    kept.extend(agent_client::env_file::read(&env)?);
    settle(&mut chosen, &kept.into_iter().collect::<Vec<_>>());
    let secrets: Vec<(&str, &str)> = (chosen.iter())
        .flat_map(|f| f.secrets.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .collect();
    if !secrets.is_empty() {
        agent_client::env_file::set(&env, &secrets)?;
    }
    for server in &chosen {
        agent_client::config::set_table(
            &config,
            &format!("sonny.mcp_servers.{}", server.name),
            Some(&server.keys),
        )?;
    }
    Ok(chosen)
}

/// What `sonny mcp import` and setup show for a server.
fn row(found: &Found) -> Row {
    let what = match (found.keys.get("command"), found.keys.get("url")) {
        (Some(command), _) => {
            let args = (found.keys.get("args").and_then(toml::Value::as_array))
                .into_iter()
                .flatten()
                .filter_map(toml::Value::as_str);
            std::iter::once(command.as_str().unwrap_or(""))
                .chain(args)
                .collect::<Vec<_>>()
                .join(" ")
        }
        (_, Some(url)) => url.as_str().unwrap_or("").to_owned(),
        _ => String::new(),
    };
    let reads = match found.needs.is_empty() {
        true => String::new(),
        false => format!(" · reads {}", found.needs.join(", ")),
    };
    let secrets = match found.secrets.len() {
        0 => String::new(),
        n => format!(
            " · {n} secret{} to ~/.agent/env",
            if n == 1 { "" } else { "s" }
        ),
    };
    let row = Row::new(
        &found.name,
        format!(
            "from {}{}{secrets}{reads} · {what}",
            found.from,
            if found.folder { " in this folder" } else { "" }
        ),
    );
    match found.keys.get("enabled").and_then(toml::Value::as_bool) {
        Some(false) => row.badge("off"),
        _ => row,
    }
}

/// The servers found here, unless Sonny's config names them already.
pub fn here() -> Result<Vec<Found>, String> {
    here_from(&[])
}

/// [`here`] of `from`'s agents alone, or of every agent's.
fn here_from(from: &[&str]) -> Result<Vec<Found>, String> {
    let home = PathBuf::from(std::env::var_os("HOME").ok_or("set HOME")?);
    let folder = std::env::current_dir().map_err(|e| e.to_string())?;
    Ok(found(
        &home,
        &folder,
        &|name| std::env::var(name).ok(),
        from,
    ))
}

/// The servers found here that the person ticks, all ticked to start,
/// brought in; with `all`, every one without asking. What each did not
/// bring is said. The names brought.
pub fn offer(all: bool) -> Result<Vec<String>, String> {
    offer_from(all, &[], &|line| println!("{line}"))
}

/// [`offer`] of the servers of `agents` alone (`codex`, `claude`,
/// `opencode`, `kiro`), or of every agent's when it names none; `say`
/// takes what it says about each.
pub fn offer_from(all: bool, agents: &[&str], say: &dyn Fn(&str)) -> Result<Vec<String>, String> {
    let unknown: Vec<&&str> = (agents.iter())
        .filter(|agent| short(agent).is_none())
        .collect();
    if !unknown.is_empty() {
        return Err(format!(
            "no agent {}: name codex, claude, opencode or kiro",
            unknown
                .iter()
                .map(|a| a.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let from: Vec<&str> = agents.iter().filter_map(|agent| short(agent)).collect();
    let (bringable, refused): (Vec<Found>, Vec<Found>) =
        (here_from(&from)?.into_iter()).partition(|f| !f.keys.is_empty());
    let (bringable, unasked): (Vec<Found>, Vec<Found>) = match all {
        true => bringable.into_iter().partition(|f| !f.folder),
        false => (bringable, Vec::new()),
    };
    let chosen: Vec<Found> = match all || bringable.is_empty() {
        true => bringable,
        false => {
            let rows: Vec<Row> = bringable.iter().map(row).collect();
            let ticked = bringable.iter().map(|f| !f.folder).collect();
            let picked =
                picker::pick_many("Bring MCP servers from your other agents", &rows, ticked)?;
            (bringable.into_iter().enumerate())
                .filter(|(index, _)| picked.contains(index))
                .map(|(_, found)| found)
                .collect()
        }
    };
    let brought = bring(chosen)?;
    for server in &brought {
        let unset = (server.needs.iter())
            .filter(|var| agent_client::env_file::var(var).is_none())
            .map(|var| format!("set {var} in ~/.agent/env; it is not set"));
        for note in server.notes.iter().cloned().chain(unset) {
            say(&format!("  {}: {note}", server.name));
        }
    }
    for server in &unasked {
        say(&format!(
            "\x1b[2mNot bringing {} from {} in this folder without asking: `sonny mcp import` offers it\x1b[0m",
            server.name, server.from
        ));
    }
    for server in &refused {
        let why = server.notes.join("; ");
        let why = why.strip_prefix("not brought in: ").unwrap_or(&why);
        say(&format!(
            "\x1b[2mNot bringing {} from {}: {why}\x1b[0m",
            server.name, server.from
        ));
    }
    Ok(brought.into_iter().map(|f| f.name).collect())
}

/// The agent a short name, as `claude`, names.
fn short(name: &str) -> Option<&'static str> {
    match name.to_ascii_lowercase().as_str() {
        "codex" => Some("Codex"),
        "claude" | "claude-code" => Some("Claude Code"),
        "opencode" => Some("OpenCode"),
        "kiro" => Some("Kiro"),
        _ => None,
    }
}

/// What `sonny mcp import --json` prints for `found`: never a secret's
/// value, only the variable it would go to.
pub fn json(found: &[Found]) -> Value {
    Value::Array(
        found
            .iter()
            .map(|f| {
                serde_json::json!({
                    "name": f.name,
                    "from": f.from,
                    "server": f.keys,
                    "secrets": f.secrets.iter().map(|(var, _)| var).collect::<Vec<_>>(),
                    "needs": f.needs,
                    "notes": f.notes,
                    "folder": f.folder,
                })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn each_agents_shape_reads_and_secrets_move_out() {
        let github = claude(
            "github",
            &json!({"command": "npx", "args": ["-y", "server-github"],
                    "env": {"GITHUB_PERSONAL_ACCESS_TOKEN": "ghp_x", "LOG_LEVEL": "info", "HOME_DIR": "${HOME_DIR}"}}),
        );
        let github = translate("Claude Code", github);
        assert_eq!(github.keys["command"].as_str(), Some("npx"));
        assert_eq!(github.keys["env"]["LOG_LEVEL"].as_str(), Some("info"));
        assert!(
            github.keys["env"]
                .get("GITHUB_PERSONAL_ACCESS_TOKEN")
                .is_none()
        );
        assert!(github.keys["env"].get("HOME_DIR").is_none());
        assert_eq!(
            github.secrets,
            vec![("GITHUB_PERSONAL_ACCESS_TOKEN".into(), "ghp_x".into())]
        );

        let linear = translate(
            "Claude Code",
            claude(
                "linear.app",
                &json!({"type": "http", "url": "https://mcp.linear.app/mcp",
                                         "headers": {"Authorization": "Bearer lin_x", "X-Team": "eng"}}),
            ),
        );
        assert_eq!(linear.name, "linear-app");
        assert_eq!(
            linear.keys["bearer_token_env_var"].as_str(),
            Some("SONNY_MCP_LINEAR_APP_TOKEN")
        );
        assert_eq!(
            linear.secrets,
            vec![("SONNY_MCP_LINEAR_APP_TOKEN".into(), "lin_x".into())]
        );
        assert_eq!(linear.keys["http_headers"]["X-Team"].as_str(), Some("eng"));

        let local = translate(
            "OpenCode",
            opencode(
                "fs",
                &json!({"type": "local", "command": ["npx", "-y", "fs"], "enabled": false,
                                   "environment": {"API_KEY": "{env:API_KEY}"}}),
            ),
        );
        assert_eq!(local.keys["args"].as_array().map(Vec::len), Some(2));
        assert_eq!(local.keys["enabled"].as_bool(), Some(false));
        assert!(local.secrets.is_empty() && local.keys.get("env").is_none());

        let remote = translate(
            "Kiro",
            claude(
                "api",
                &json!({"url": "https://api.example.com/mcp",
                                  "headers": {"Authorization": "Bearer ${API_TOKEN}"}, "disabledTools": ["drop"]}),
            ),
        );
        assert_eq!(
            remote.keys["bearer_token_env_var"].as_str(),
            Some("API_TOKEN")
        );
        assert!(remote.secrets.is_empty());
        assert_eq!(
            remote.keys["disabled_tools"].as_array().map(Vec::len),
            Some(1)
        );

        let sse = translate(
            "Claude Code",
            claude("old", &json!({"type": "sse", "url": "https://x"})),
        );
        assert!(sse.keys.is_empty() && sse.notes[0].contains("SSE"));

        let toml: toml::Table = "[mcp_servers.docs]\ncommand = \"docs-mcp\"\nenv_vars = [\"DOCS_TOKEN\"]\nstartup_timeout_sec = 20\n".parse().unwrap();
        let docs = translate(
            "Codex",
            codex("docs", &toml["mcp_servers"]["docs"]).unwrap(),
        );
        assert_eq!(docs.keys["startup_timeout_sec"].as_integer(), Some(20));
        assert!(docs.keys.get("env").is_none() && docs.secrets.is_empty());
    }

    #[test]
    fn files_in_home_and_folder_are_found_once_by_name_codex_first() {
        let root = std::env::temp_dir().join(format!("sonny-mcp-import-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (home, folder) = (root.join("home"), root.join("work"));
        for dir in [
            home.join(".codex"),
            home.join(".kiro/settings"),
            home.join(".config/opencode"),
            folder.clone(),
        ] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(
            home.join(".codex/config.toml"),
            "[mcp_servers.docs]\ncommand = \"codex-docs\"\n",
        )
        .unwrap();
        std::fs::write(
            home.join(".claude.json"),
            json!({"mcpServers": {"docs": {"command": "claude-docs"}},
                   "projects": {folder.to_string_lossy(): {"mcpServers": {"local": {"command": "here"}}}}})
                .to_string(),
        )
        .unwrap();
        std::fs::write(
            folder.join(".mcp.json"),
            r#"{"mcpServers": {"shared": {"command": "team"}}}"#,
        )
        .unwrap();
        std::fs::write(
            home.join(".config/opencode/opencode.jsonc"),
            "{\n  // mine\n  \"mcp\": {\"oc\": {\"type\": \"remote\", \"url\": \"https://oc/mcp\"}}\n}\n",
        )
        .unwrap();
        std::fs::write(
            home.join(".kiro/settings/mcp.json"),
            r#"{"mcpServers": {"kiro": {"command": "k"}}}"#,
        )
        .unwrap();
        let mut names: Vec<(String, &str)> = Vec::new();
        for agent in AGENTS {
            for raw in read(agent, &home, &folder, &|_| None) {
                // Only the folder's own file is the folder's.
                assert_eq!(raw.folder, raw.name == "shared", "{}", raw.name);
                names.push((raw.name, agent));
            }
        }
        assert!(names.contains(&("docs".into(), "Codex")));
        assert!(names.contains(&("local".into(), "Claude Code")));
        assert!(names.contains(&("shared".into(), "Claude Code")));
        assert!(names.contains(&("oc".into(), "OpenCode")));
        assert!(names.contains(&("kiro".into(), "Kiro")));
        // Codex's docs comes first; Claude Code's alone is Claude Code's.
        let docs = |from: &[&str]| {
            (found(&home, &folder, &|_| None, from).into_iter())
                .find(|f| f.name == "docs")
                .map(|f| f.from)
        };
        assert_eq!(docs(&[]), Some("Codex"));
        assert_eq!(docs(&["Claude Code"]), Some("Claude Code"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_secret_env_holds_otherwise_stays_with_its_server() {
        let mut chosen = vec![
            translate(
                "Codex",
                claude("a", &json!({"command": "a", "env": {"API_KEY": "one"}})),
            ),
            translate(
                "Kiro",
                claude("b", &json!({"command": "b", "env": {"API_KEY": "two"}})),
            ),
            translate(
                "Kiro",
                claude("c", &json!({"command": "c", "env": {"GH_TOKEN": "same"}})),
            ),
        ];
        settle(&mut chosen, &[("GH_TOKEN".into(), "same".into())]);
        assert_eq!(chosen[0].secrets.len(), 1);
        assert!(chosen[1].secrets.is_empty());
        assert_eq!(chosen[1].keys["env"]["API_KEY"].as_str(), Some("two"));
        assert_eq!(chosen[2].secrets.len(), 1);
        assert!(chosen[2].keys.get("env").is_none());

        // A URL server takes no env: its token gets a free variable.
        let remote = |name: &str, token: &str| {
            let headers = json!({"Authorization": format!("Bearer {token}")});
            translate(
                "Claude Code",
                claude(
                    name,
                    &json!({"type": "http", "url": "https://x", "headers": headers}),
                ),
            )
        };
        let mut chosen = vec![remote("foo-bar", "one"), remote("foo_bar", "two")];
        settle(
            &mut chosen,
            &[("SONNY_MCP_FOO_BAR_TOKEN_2".into(), "three".into())],
        );
        assert_eq!(
            chosen[1].keys["bearer_token_env_var"].as_str(),
            Some("SONNY_MCP_FOO_BAR_TOKEN_3")
        );
        assert_eq!(
            chosen[1].secrets,
            vec![("SONNY_MCP_FOO_BAR_TOKEN_3".into(), "two".into())]
        );
        assert!(chosen.iter().all(|found| found.keys.get("env").is_none()));
    }

    #[test]
    fn a_codex_literal_bearer_moves_out_of_the_config() {
        let server: toml::Value = toml::from_str(
            "url = \"https://x\"\nhttp_headers = { Authorization = \"Bearer sk\", X-Team = \"a\" }",
        )
        .unwrap();
        let found = translate("Codex", codex("docs", &server).unwrap());
        assert_eq!(
            found.keys["bearer_token_env_var"].as_str(),
            Some("SONNY_MCP_DOCS_TOKEN")
        );
        assert_eq!(found.keys["http_headers"]["X-Team"].as_str(), Some("a"));
        assert!(found.keys["http_headers"].get("Authorization").is_none());
        assert!(!json(&[found]).to_string().contains("sk"));
    }

    #[test]
    fn variables_and_secrets_are_told_apart() {
        assert_eq!(named("${A_B}"), Some("A_B"));
        assert_eq!(named("{env:X}"), Some("X"));
        assert_eq!(named("${X:-y}"), Some("X"));
        assert_eq!(named("prefix ${X}"), None);
        assert!(
            secret("GITHUB_PERSONAL_ACCESS_TOKEN")
                && secret("OPENAI_API_KEY")
                && secret("x-api-key")
        );
        assert!(!secret("LOG_LEVEL") && !secret("X-Team"));
        assert_eq!(
            uncommented("{\"a\": \"//not\" /* c */ } // d"),
            "{\"a\": \"//not\"  } "
        );
        assert_eq!(
            uncommented("{\"a\": [1, 2,], \"b\": \",}\",\n}"),
            "{\"a\": [1, 2], \"b\": \",}\"\n}"
        );
        assert_eq!(
            uncommented("{\"d\": {\"u\": 1}, /* c */}"),
            "{\"d\": {\"u\": 1} }"
        );
    }
}
