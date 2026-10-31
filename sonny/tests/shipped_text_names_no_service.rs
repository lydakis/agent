//! What Sonny ships for its agents to read, its skills (sonny/skills) and
//! roles (sonny/agents), teaches how working with agents goes in Sonny and
//! names nothing of the person's own world: no service, host or web
//! address. The person brings those, through memory, their own skills and
//! profiles, MCP servers and lookout sources.
use std::path::{Path, PathBuf};

/// Services a person may use, matched as written.
const SERVICES: [&str; 34] = [
    "GitHub",
    "Github",
    "GitLab",
    "Gitlab",
    "Bitbucket",
    "Slack",
    "Discord",
    "Telegram",
    "WhatsApp",
    "Jira",
    "Confluence",
    "Linear",
    "Asana",
    "Trello",
    "Notion",
    "Figma",
    "Google",
    "Gmail",
    "Outlook",
    "Dropbox",
    "iCloud",
    "OneDrive",
    "S3",
    "Azure",
    "GCP",
    "Vercel",
    "Netlify",
    "Heroku",
    "Cloudflare",
    "Sentry",
    "Datadog",
    "PagerDuty",
    "Zoom",
    "Tailscale",
];

/// The endings a host name has, leaving out those a file's name has too
/// (`.sh`, `.app`).
const DOMAINS: [&str; 7] = [".com", ".org", ".net", ".io", ".dev", ".ai", ".cloud"];

/// Addresses that name no service: the SVG namespace a picture declares.
const ALLOWED: [&str; 1] = ["http://www.w3.org/2000/svg"];

fn files(folder: &Path, found: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(folder).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files(&path, found);
        } else {
            found.push(path);
        }
    }
}

/// What on `line` names a service, a host or a web address.
fn named(line: &str) -> Vec<String> {
    let mut line = line.to_owned();
    for allowed in ALLOWED {
        line = line.replace(allowed, "");
    }
    let mut found = Vec::new();
    for word in line.split(|c: char| c.is_whitespace() || "()<>[]{}\"'`,;".contains(c)) {
        if word.contains("://") {
            found.push(word.trim_end_matches(['.', ':']).to_owned());
            continue;
        }
        for part in word.split(|c: char| !(c.is_alphanumeric() || ".-_".contains(c))) {
            let part = part.trim_end_matches('.');
            let host = DOMAINS
                .iter()
                .find_map(|domain| part.strip_suffix(domain))
                .is_some_and(|name| !name.is_empty() && !name.ends_with('.'));
            if SERVICES.contains(&part) || host {
                found.push(part.to_owned());
            }
        }
    }
    found
}

#[test]
fn shipped_skills_and_roles_name_no_service_or_host() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut shipped = Vec::new();
    files(&root.join("skills"), &mut shipped);
    files(&root.join("agents"), &mut shipped);
    assert!(shipped.len() > 3, "found {shipped:?}");
    let mut names = Vec::new();
    for file in shipped {
        let relative = file.strip_prefix(root).unwrap().display().to_string();
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        for (number, line) in text.lines().enumerate() {
            for name in named(line) {
                names.push(format!("{relative}:{}: {name}", number + 1));
            }
        }
    }
    assert!(
        names.is_empty(),
        "shipped skills and roles name the person's world; teach the door \
         (memory, their skills, MCP, lookout) instead:\n{}",
        names.join("\n")
    );
}

#[test]
fn services_hosts_and_addresses_are_found() {
    assert_eq!(named("open a PR on GitHub"), ["GitHub"]);
    assert_eq!(named("post it to the team's Slack."), ["Slack"]);
    assert_eq!(
        named("see https://example.org/x for more"),
        ["https://example.org/x"]
    );
    assert_eq!(named("push to git.example.com:repo"), ["git.example.com"]);
    assert_eq!(
        named(r#"<svg xmlns="http://www.w3.org/2000/svg">"#),
        Vec::<String>::new()
    );
    // Words that only look like one: a file, a linear search, a version.
    assert!(named("edit lead.md, run setup.sh, a linear scan, Sonny 0.7.0, draws").is_empty());
}
