//! Subscriptions: what the lookout watches for each subscriber. Each
//! subscriber's are one file, `STORE.lookout/BOT.json`, which its
//! `subscriptions` tool edits; the lookout reads them again every few
//! seconds. The lookout watches for whoever has a file and knows no leads:
//! Sonny writes a lead's `tasks` subscription when it opens the lead's
//! project (`lead`, `ensure`).
use agent_client::config::COMMONS_SOURCE;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub struct Sub {
    pub name: String,
    /// Bots whose tasks it watches: every bot one of them or its tasks made.
    pub tasks_of: Vec<String>,
    /// Bots it watches by name, `*` matching any run of characters.
    pub bots: Vec<String>,
    /// Sources it hears, by the names `[sonny.sources]` gives them: every
    /// event one prints, or as `NAME:TOPIC` its events on that topic; of
    /// those, the ones its filter matches.
    pub sources: Vec<String>,
    /// An event pattern on the fields of its sources' events, as
    /// `pattern` reads it: only those it matches are heard.
    pub filter: Option<Value>,
    /// The bot it was made for, by ID, when Sonny made it on the bot's
    /// behalf: another bot that takes the name does not hold it.
    pub owner: Option<i64>,
}

impl Sub {
    /// What Sonny subscribes a lead to: every bot it or its tasks made;
    /// and for the folder's first lead, `{project}.lead`, every bot named in
    /// the project's way too, so a bot no agent made reaches a lead. Another
    /// lead of the folder hears its own tasks only, never the others'.
    pub fn lead(lead: &str, project: &str) -> Sub {
        let first = lead == format!("{project}.lead");
        Sub {
            name: "tasks".into(),
            tasks_of: vec![lead.to_owned()],
            bots: match first {
                true => vec![format!("{project}.*")],
                false => Vec::new(),
            },
            sources: Vec::new(),
            filter: None,
            owner: None,
        }
    }

    /// What posting in a commons thread subscribes an agent to: the
    /// thread's new posts, as the commons source tells them, for the bot
    /// of that ID.
    pub fn commons(thread: &str, owner: Option<i64>) -> Sub {
        Sub {
            name: format!("{COMMONS_SOURCE}.{thread}"),
            tasks_of: Vec::new(),
            bots: Vec::new(),
            sources: vec![format!("{COMMONS_SOURCE}:{thread}")],
            filter: None,
            owner,
        }
    }

    /// Whether the bot of `record` holds it: one made for another bot of
    /// its name does not.
    pub fn held_by(&self, record: &Value) -> bool {
        self.owner
            .is_none_or(|owner| record["bot_id"].as_i64().is_none_or(|id| id == owner))
    }

    /// Whether it hears this event of a source: it names the source, or
    /// the source and the event's topic, and its filter, if any, matches.
    pub fn hears(&self, event: &super::sources::Event) -> bool {
        let named = |name: &String| match super::sources::named(name) {
            (source, None) => source == event.source,
            (source, topic) => source == event.source && topic == event.topic.as_deref(),
        };
        self.sources.iter().any(named)
            && (self.filter.as_ref())
                .is_none_or(|filter| super::pattern::matches(filter, &event.fields))
    }

    pub fn to_json(&self) -> Value {
        let mut out = json!({"name": self.name});
        if !self.tasks_of.is_empty() {
            out["tasks_of"] = json!(self.tasks_of);
        }
        if !self.bots.is_empty() {
            out["bots"] = json!(self.bots);
        }
        if !self.sources.is_empty() {
            out["sources"] = json!(self.sources);
        }
        if let Some(owner) = self.owner {
            out["bot_id"] = json!(owner);
        }
        if let Some(filter) = &self.filter {
            out["filter"] = filter.clone();
        }
        out
    }

    pub fn from_json(value: &Value) -> Option<Sub> {
        let strings = |key: &str| -> Vec<String> {
            (value[key].as_array().into_iter())
                .flatten()
                .filter_map(|s| s.as_str().map(str::to_owned))
                .collect()
        };
        Some(Sub {
            // A file edited by hand is held to what `add` takes.
            name: value["name"]
                .as_str()
                .filter(|n| named_plainly(n))?
                .to_owned(),
            tasks_of: strings("tasks_of"),
            bots: strings("bots"),
            sources: strings("sources"),
            // A filter that does not check hears nothing rather than all.
            filter: match value.get("filter") {
                None | Some(Value::Null) => None,
                Some(filter) => match super::pattern::check(filter) {
                    Ok(()) => Some(filter.clone()),
                    Err(_) => Some(Value::Bool(false)),
                },
            },
            owner: value["bot_id"].as_i64(),
        })
    }

    /// One line on it, as `/lookout` lists it.
    pub fn describe(&self) -> String {
        let watched: Vec<String> = (self.tasks_of.iter().map(|b| format!("tasks of {b}")))
            .chain(self.bots.iter().map(|g| format!("bots {g}")))
            .chain(self.sources.iter().map(|s| format!("source {s}")))
            .collect();
        let filter = match &self.filter {
            Some(filter) => format!(" where {filter}"),
            None => String::new(),
        };
        format!("{} · {}{filter}", self.name, watched.join(", "))
    }

    /// The first field in which two subscriptions under one name differ.
    fn differs(&self, other: &Sub) -> Option<&'static str> {
        [
            ("tasks_of", self.tasks_of != other.tasks_of),
            ("bot", self.bots != other.bots),
            ("sources", self.sources != other.sources),
            ("filter", self.filter != other.filter),
        ]
        .into_iter()
        .find_map(|(field, differs)| differs.then_some(field))
    }
}

/// Whether `glob` (`*` for any run of characters) matches all of `name`.
pub fn matches(glob: &str, name: &str) -> bool {
    let mut parts = glob.split('*');
    let first = parts.next().unwrap_or("");
    let Some(mut rest) = name.strip_prefix(first) else {
        return false;
    };
    let parts: Vec<&str> = parts.collect();
    for (n, part) in parts.iter().enumerate() {
        if n + 1 == parts.len() {
            return rest.ends_with(part);
        }
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    rest.is_empty()
}

/// Where subscribers' files are: beside the store.
pub fn dir(store: &Path) -> PathBuf {
    let mut path = store.as_os_str().to_owned();
    path.push(".lookout");
    PathBuf::from(path)
}

fn file(store: &Path, subscriber: &str) -> PathBuf {
    dir(store).join(format!("{subscriber}.json"))
}

/// A subscriber's subscriptions, as its file holds them.
pub fn read(store: &Path, subscriber: &str) -> Vec<Sub> {
    let Ok(text) = std::fs::read_to_string(file(store, subscriber)) else {
        return Vec::new();
    };
    let value: Value = serde_json::from_str(&text).unwrap_or_default();
    (value["subscriptions"].as_array().into_iter())
        .flatten()
        .filter_map(Sub::from_json)
        .collect()
}

/// The subscriber's file as it is, none when it has none: one it cannot
/// read, or that holds what is not a subscription, is refused, so a
/// change never writes over what it could not read.
fn read_whole(store: &Path, subscriber: &str) -> Result<Vec<Sub>, String> {
    let path = file(store, subscriber);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let unreadable = || format!("{}: not a subscriptions file", path.display());
    let value: Value = serde_json::from_str(&text).map_err(|_| unreadable())?;
    (value["subscriptions"]
        .as_array()
        .ok_or_else(unreadable)?
        .iter())
    .map(|sub| Sub::from_json(sub).ok_or_else(unreadable))
    .collect()
}

/// Every subscriber with a file, and what it holds.
pub fn all(store: &Path) -> Vec<(String, Vec<Sub>)> {
    let Ok(entries) = std::fs::read_dir(dir(store)) else {
        return Vec::new();
    };
    let mut all: Vec<(String, Vec<Sub>)> = entries
        .filter_map(|entry| {
            let name = entry.ok()?.file_name().to_string_lossy().into_owned();
            let bot = name.strip_suffix(".json")?.to_owned();
            let subs = read(store, &bot);
            Some((bot, subs))
        })
        .filter(|(_, subs)| !subs.is_empty())
        .collect();
    all.sort_by(|a, b| a.0.cmp(&b.0));
    all
}

/// Add `sub` to the subscriber's file unless one by its name is there,
/// which is kept as it is: whether it was added.
pub fn ensure(store: &Path, subscriber: &str, sub: Sub) -> Result<bool, String> {
    let _held = held(store)?;
    let mut subs = read_whole(store, subscriber)?;
    // One made for another bot of the name gives way to this bot's own.
    let theirs = |s: &Sub| {
        s.name == sub.name && s.owner.is_some() && sub.owner.is_some() && s.owner != sub.owner
    };
    subs.retain(|s| !theirs(s));
    if subs.iter().any(|s| s.name == sub.name) {
        return Ok(false);
    }
    subs.insert(0, sub);
    write(store, subscriber, &subs)?;
    Ok(true)
}

/// The longest name a subscription may have: a commons thread's, 64,
/// after `commons.`.
const NAME_MAX: usize = 80;

/// The longest name the daemon gives a bot.
const BOT_NAME_MAX: usize = 128;

/// A name the note may say as it is: it reaches the approver as the
/// person's words, so it holds nothing that reads as more.
fn named_plainly(name: &str) -> bool {
    plain(name, NAME_MAX)
}

fn plain(name: &str, max: usize) -> bool {
    !name.is_empty()
        && name.len() <= max
        && !name.starts_with('.')
        && (name.bytes()).all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// The lock every edit holds while it reads and writes.
fn held(store: &Path) -> Result<std::fs::File, String> {
    use std::os::fd::AsRawFd;
    std::fs::create_dir_all(dir(store)).map_err(|e| e.to_string())?;
    let path = dir(store).join(".lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    // SAFETY: flock on a descriptor `file` owns and keeps open; the lock goes
    // with the file.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(format!(
            "{}: {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    Ok(file)
}

fn write(store: &Path, subscriber: &str, subs: &[Sub]) -> Result<(), String> {
    let path = file(store, subscriber);
    if subs.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(format!("{}: {e}", path.display()))
            }
            _ => Ok(()),
        };
    }
    std::fs::create_dir_all(dir(store)).map_err(|e| e.to_string())?;
    let body = json!({"subscriptions": subs.iter().map(Sub::to_json).collect::<Vec<_>>()});
    // Written whole, then moved in, so the lookout never reads half a file.
    let partial = path.with_extension(format!("json.{}.partial", std::process::id()));
    std::fs::write(&partial, format!("{body:#}\n")).map_err(|e| e.to_string())?;
    std::fs::rename(&partial, &path).map_err(|e| e.to_string())
}

/// Remove the subscriber's subscription by that name: whether it had one.
pub fn remove(store: &Path, subscriber: &str, name: &str) -> Result<bool, String> {
    let _held = held(store)?;
    let mut subs = read(store, subscriber);
    drop_named(store, subscriber, &mut subs, name)
}

fn drop_named(
    store: &Path,
    subscriber: &str,
    subs: &mut Vec<Sub>,
    name: &str,
) -> Result<bool, String> {
    let before = subs.len();
    subs.retain(|s| s.name != name);
    write(store, subscriber, subs)?;
    Ok(subs.len() < before)
}

/// A `subscriptions` call of `subscriber`'s: `add` one, as `to_json`
/// writes it, or `remove` one by name, then list them all, with the
/// `sources` the person set up. Adding one again changes nothing; one
/// under a name in use that differs is refused with the field, and one
/// naming a source there is not with the sources there are.
pub fn change(
    store: &Path,
    subscriber: &str,
    add: Option<&Value>,
    remove: Option<&str>,
    sources: &[String],
) -> Result<Value, String> {
    if !plain(subscriber, BOT_NAME_MAX) {
        return Err(format!("{subscriber:?} is not a bot's name"));
    }
    let add = add.map(parse).transpose()?;
    let known = |name: &String| {
        let (source, topic) = super::sources::named(name);
        sources.iter().any(|s| s == source) && topic.is_none_or(|topic| plain(topic, NAME_MAX))
    };
    if let Some(unknown) = (add.iter().flat_map(|sub| &sub.sources)).find(|s| !known(s)) {
        let there = match sources.is_empty() {
            true => "none: the person sets them up under [sonny.sources] in ~/.agent/config.toml"
                .to_owned(),
            false => sources.join(", "),
        };
        return Err(format!(
            "no source {unknown:?}; the sources there are {there}"
        ));
    }
    // One change at a time, so two at once both land.
    let _held = held(store)?;
    let mut subs = read_whole(store, subscriber)?;
    let mut said = json!({});
    if let Some(name) = remove {
        said["removed"] = json!(drop_named(store, subscriber, &mut subs, name)?);
    }
    if let Some(sub) = &add
        && let Some(filter) = &sub.filter
    {
        let unseen = unseen(store, sub, filter);
        if !unseen.is_empty() {
            said["unseen"] = json!(format!(
                "no recent event of {} had {}: the filter hears none until one does",
                sub.sources.join(" or "),
                unseen.join(", ")
            ));
        }
    }
    if let Some(sub) = add {
        match subs.iter().find(|s| s.name == sub.name) {
            Some(had) => match had.differs(&sub) {
                None => said["added"] = json!(false),
                Some(field) => {
                    return Err(format!(
                        "a subscription {} with another {field} is there; remove it first",
                        sub.name
                    ));
                }
            },
            None => {
                subs.push(sub);
                write(store, subscriber, &subs)?;
                said["added"] = json!(true);
            }
        }
    }
    said["subscriptions"] = subs.iter().map(Sub::to_json).collect();
    said["sources"] = json!(super::sources::listed(store, sources));
    Ok(said)
}

/// The fields that keep a filter from hearing any event like those its
/// sources lately printed, where the lookout has seen one.
fn unseen(store: &Path, sub: &Sub, filter: &Value) -> Vec<String> {
    let seen: Vec<Vec<String>> = (sub.sources.iter())
        .filter_map(|source| super::sources::fields_seen(store, super::sources::named(source).0))
        .collect();
    if seen.is_empty() {
        return Vec::new();
    }
    let had = |field: &str| {
        seen.iter().flatten().any(|had| {
            had == field || (had.strip_prefix(field)).is_some_and(|r| r.starts_with('.'))
        })
    };
    super::pattern::missing(filter, &had)
}

/// A subscription as a call gives it, held to what one may say.
fn parse(value: &Value) -> Result<Sub, String> {
    let name = value["name"].as_str().unwrap_or_default();
    if !named_plainly(name) {
        return Err(format!(
            "{name:?}: a subscription's name is letters, digits, '.', '_' and '-', up to {NAME_MAX}"
        ));
    }
    let strings = |key: &str| -> Vec<String> {
        (value[key].as_array().into_iter())
            .flatten()
            .filter_map(|s| s.as_str().map(str::to_owned))
            .collect()
    };
    let filter = match value.get("filter") {
        None | Some(Value::Null) => None,
        Some(filter) => {
            super::pattern::check(filter)?;
            Some(filter.clone())
        }
    };
    let sub = Sub {
        name: name.to_owned(),
        tasks_of: strings("tasks_of"),
        bots: strings("bots"),
        sources: strings("sources"),
        filter,
        owner: None,
    };
    if sub.tasks_of.is_empty() && sub.bots.is_empty() && sub.sources.is_empty() {
        return Err("a subscription watches tasks_of a bot, bots by name or sources".into());
    }
    if sub.filter.is_some() && sub.sources.is_empty() {
        return Err("a filter narrows what sources say: name the sources it filters".into());
    }
    Ok(sub)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_it_cannot_read_is_never_written_over() {
        let store = std::env::temp_dir().join(format!("subs-torn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(dir(&store));
        std::fs::create_dir_all(dir(&store)).unwrap();
        let path = file(&store, "calc.lead");
        std::fs::write(&path, "{torn").unwrap();
        assert!(ensure(&store, "calc.lead", Sub::commons("t", None)).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{torn");
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            ensure(&store, "calc.lead", Sub::commons("t", Some(1))),
            Ok(true)
        );
        assert_eq!(
            ensure(&store, "calc.lead", Sub::commons("t", Some(1))),
            Ok(false)
        );
        // Another bot of the name, posting there, has its own.
        assert_eq!(
            ensure(&store, "calc.lead", Sub::commons("t", Some(2))),
            Ok(true)
        );
        assert_eq!(read(&store, "calc.lead")[0].owner, Some(2));
        let theirs = Sub::commons("t", Some(2));
        assert!(theirs.held_by(&json!({"bot_id": 2})));
        assert!(!theirs.held_by(&json!({"bot_id": 1})));
        assert!(Sub::commons("t", None).held_by(&json!({"bot_id": 1})));
        let _ = std::fs::remove_dir_all(dir(&store));
    }

    #[test]
    fn another_lead_of_a_folder_hears_its_own_tasks_only() {
        assert_eq!(Sub::lead("shop.lead", "shop").bots, ["shop.*"]);
        let other = Sub::lead("shop.lead.billing", "shop");
        assert!(other.bots.is_empty());
        assert_eq!(other.tasks_of, ["shop.lead.billing"]);
    }

    #[test]
    fn globs_match_whole_names() {
        assert!(matches("*.lead", "shop.lead"));
        assert!(matches("shop.*", "shop.fix.review"));
        assert!(matches("shop.lead", "shop.lead"));
        assert!(!matches("shop.lead", "shop.lead2"));
        assert!(!matches("*.lead", "shop.leader"));
        assert!(matches("a*b*c", "aXbYc"));
        assert!(!matches("a*b*c", "aXcYb"));
    }

    #[test]
    fn a_subscription_to_sources_hears_every_event() {
        let chat = parse(&json!({"name": "chat", "sources": ["slack", "mail"]})).unwrap();
        assert_eq!(chat.describe(), "chat · source slack, source mail");
        assert_eq!(Sub::from_json(&chat.to_json()), Some(chat));
    }

    #[test]
    fn a_filter_narrows_sources_and_warns_of_fields_no_event_had() {
        let dir = std::env::temp_dir().join(format!("lookout-subs-filter-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = dir.join("state.sqlite");
        let sources = ["cr".to_owned()];
        let add = |sub: Value| change(&store, "shop.lead", Some(&sub), None, &sources);
        let filter = json!({"paths": [{"prefix": "src/billing/"}]});
        let crs = json!({"name": "crs", "sources": ["cr"], "filter": filter});
        // No event seen yet: nothing to warn of.
        let added = add(crs.clone()).unwrap();
        assert_eq!(added["subscriptions"][0]["filter"], filter);
        assert!(added.get("unseen").is_none(), "{added}");
        let sub = &read(&store, "shop.lead")[0];
        assert_eq!(sub.filter.as_ref(), Some(&filter));
        assert!(
            sub.describe()
                .ends_with(r#"source cr where {"paths":[{"prefix":"src/billing/"}]}"#)
        );
        // Another filter under the name is another subscription.
        let other = json!({"name": "crs", "sources": ["cr"], "filter": {"paths": ["x"]}});
        assert!(add(other).unwrap_err().contains("another filter"));
        // A filter is for sources, and checked.
        let bots = json!({"name": "b", "bots": ["a"], "filter": filter});
        assert!(add(bots).unwrap_err().contains("name the sources"));
        let bad = json!({"name": "b", "sources": ["cr"], "filter": {"paths": "src/"}});
        assert!(add(bad).unwrap_err().contains("list of values"));
        // Once the lookout has seen the source's shape, a field it never
        // had is said.
        let shapes = super::super::beside(&store, ".sources").join(".shapes");
        std::fs::create_dir_all(&shapes).unwrap();
        std::fs::write(
            shapes.join("cr.json"),
            json!({"events": 3, "fields": {
                "text": {"kinds": ["string"], "example": "\"CR-1\""},
                "paths": {"kinds": ["list of strings"], "example": "[\"src/a.rs\"]"},
                "meta.team": {"kinds": ["string"], "example": "\"core\""}}})
            .to_string(),
        )
        .unwrap();
        let typo = json!({"name": "t", "sources": ["cr"],
            "filter": {"path": ["x"], "meta": {"team": ["core"]}}});
        let warned = add(typo).unwrap();
        assert_eq!(
            warned["unseen"],
            "no recent event of cr had path: the filter hears none until one does"
        );
        let listed = &warned["sources"][0];
        assert_eq!(listed["name"], "cr");
        assert_eq!(
            listed["fields"]["paths"],
            "list of strings, as [\"src/a.rs\"]"
        );
        // A file edited by hand with a filter that does not check hears
        // nothing rather than everything.
        let bad =
            Sub::from_json(&json!({"name": "x", "sources": ["cr"], "filter": {"a": 1}})).unwrap();
        let event =
            super::super::sources::Event::parse("cr", r#"{"text": "t", "a": 1}"#, 0).unwrap();
        assert!(!bad.hears(&event));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn subscriptions_are_added_once_and_refused_when_they_differ() {
        let dir = std::env::temp_dir().join(format!("lookout-subs-edit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = dir.join("state.sqlite");
        let add = |sub: Value| change(&store, "shop.lead", Some(&sub), None, &[]);
        let prs = json!({"name": "prs", "tasks_of": ["shop.lead"]});
        assert_eq!(add(prs.clone()).unwrap()["added"], true);
        let again = add(prs.clone()).unwrap();
        assert_eq!(again["added"], false);
        assert_eq!(again["subscriptions"].as_array().unwrap().len(), 1);
        assert_eq!(read(&store, "shop.lead").len(), 1);
        let refused =
            add(json!({"name": "prs", "tasks_of": ["shop.lead"], "bots": ["shop.*"]})).unwrap_err();
        assert!(refused.contains("another bot"), "{refused}");
        assert!(add(json!({"name": "x"})).unwrap_err().contains("watches"));
        // A source is one the person set up.
        let slack = json!({"name": "slack", "sources": ["slack"]});
        let none = add(slack.clone()).unwrap_err();
        assert!(
            none.contains("no source \"slack\"") && none.contains("[sonny.sources]"),
            "{none}"
        );
        let sources = ["mail".to_owned(), "slack".to_owned()];
        let heard = change(&store, "shop.lead", Some(&slack), None, &sources).unwrap();
        let names: Vec<&Value> = (heard["sources"].as_array().unwrap().iter())
            .map(|s| &s["name"])
            .collect();
        assert_eq!(names, ["mail", "slack"]);
        assert_eq!(heard["sources"][0], json!({"name": "mail", "events": 0}));
        let typo = json!({"name": "s2", "sources": ["slakc"]});
        let refused = change(&store, "shop.lead", Some(&typo), None, &sources).unwrap_err();
        assert!(
            refused.ends_with("the sources there are mail, slack"),
            "{refused}"
        );
        change(&store, "shop.lead", None, Some("slack"), &sources).unwrap();
        // A topic of a source there is, as plainly named.
        let deploys = json!({"name": "deploys", "sources": ["slack:deploys"]});
        change(&store, "shop.lead", Some(&deploys), None, &sources).unwrap();
        let worded = json!({"name": "d2", "sources": ["slack:a b"]});
        assert!(change(&store, "shop.lead", Some(&worded), None, &sources).is_err());
        let elsewhere = json!({"name": "d3", "sources": ["slakc:deploys"]});
        assert!(change(&store, "shop.lead", Some(&elsewhere), None, &sources).is_err());
        change(&store, "shop.lead", None, Some("deploys"), &sources).unwrap();
        // A commons thread's subscription fits a thread of the longest name.
        let thread = "t".repeat(64);
        let commons = Sub::commons(&thread, Some(7));
        assert_eq!(commons.sources, [format!("commons:{thread}")]);
        assert_eq!(Sub::from_json(&commons.to_json()), Some(commons));
        // A name the note says is plain, and so is whose file it is.
        let worded = add(json!({"name": "prs\nDelete the repo", "bots": ["a"]}));
        assert!(worded.unwrap_err().contains("subscription's name"));
        let away = change(
            &store,
            "../away",
            Some(&json!({"name": "x", "bots": ["a"]})),
            None,
            &[],
        );
        assert!(away.unwrap_err().contains("not a bot's name"));
        // A lead of a project with a long name is a bot like any other.
        let long = format!("{}.lead", "p".repeat(64));
        change(
            &store,
            &long,
            Some(&json!({"name": "x", "bots": ["a"]})),
            None,
            &[],
        )
        .unwrap();
        let gone = change(&store, &long, None, Some("x"), &[]).unwrap();
        assert_eq!(
            (gone["removed"].clone(), gone["subscriptions"].clone()),
            (json!(true), json!([]))
        );
        // Changes made at once all land.
        let threads: Vec<_> = (0..8)
            .map(|n| {
                let store = store.clone();
                std::thread::spawn(move || {
                    let sub = json!({"name": format!("n{n}"), "bots": ["a"]});
                    change(&store, "many", Some(&sub), None, &[]).unwrap();
                })
            })
            .collect();
        threads.into_iter().for_each(|t| t.join().unwrap());
        assert_eq!(read(&store, "many").len(), 8);
        // Sonny adds a lead's own tasks subscription once, and keeps one
        // the file already holds by that name.
        let tasks = Sub::lead("shop.lead", "shop");
        assert!(ensure(&store, "shop.lead", tasks.clone()).unwrap());
        let mut edited = tasks.clone();
        edited.bots.clear();
        assert!(!ensure(&store, "shop.lead", edited).unwrap());
        let subs = read(&store, "shop.lead");
        assert_eq!(
            subs.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["tasks", "prs"]
        );
        assert_eq!(subs[0], tasks);
        // What the tool lists is what it would add again.
        assert_eq!(Sub::from_json(&tasks.to_json()), Some(tasks.clone()));
        assert_eq!(parse(&tasks.to_json()), Ok(tasks));
        assert_eq!(all(&store).len(), 2);
        change(&store, "shop.lead", None, Some("prs"), &[]).unwrap();
        change(&store, "shop.lead", None, Some("tasks"), &[]).unwrap();
        assert_eq!(all(&store).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
