//! Subscriptions: what the lookout watches for each subscriber, and what it
//! asks about it. Each subscriber's are one file, `STORE.lookout/BOT.json`,
//! which `sonny lookout add`, `ls` and `rm` edit; the lookout reads them
//! again every few seconds. A lead has a `tasks` subscription of its own,
//! over its tasks with every built-in question, unless its file holds one
//! by that name.
use super::{Kind, QUESTIONS};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Something a subscriber looks for, in its own words, and what it does on
/// a yes.
#[derive(Clone, Debug, PartialEq)]
pub struct Look {
    pub text: String,
    pub then: String,
    /// A yes cuts into the subscriber's running turn.
    pub urgent: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Sub {
    pub name: String,
    /// Bots whose tasks it watches: every bot one of them or its tasks made.
    pub tasks_of: Vec<String>,
    /// Bots it watches by name, `*` matching any run of characters.
    pub bots: Vec<String>,
    pub on: Vec<Kind>,
    /// Built-in questions by ID; `None` asks every one that fits the event,
    /// unless the subscription looks for something of its own.
    pub ask: Option<Vec<String>>,
    pub look_for: Vec<Look>,
    /// A line every check reads, saying what the subscriber cares about.
    pub context: Option<String>,
}

pub const KINDS: [(&str, Kind); 4] = [
    ("said", Kind::Said),
    ("ended", Kind::Ended),
    ("waiting", Kind::Waiting),
    ("quiet", Kind::Quiet),
];

fn kind_name(kind: Kind) -> &'static str {
    KINDS
        .iter()
        .find(|(_, k)| *k == kind)
        .map_or("?", |(n, _)| n)
}

impl Sub {
    /// A lead's own: its tasks, every event, every built-in.
    pub fn tasks(lead: &str) -> Sub {
        Sub {
            name: "tasks".into(),
            tasks_of: vec![lead.to_owned()],
            bots: Vec::new(),
            on: KINDS.iter().map(|(_, k)| *k).collect(),
            ask: None,
            look_for: Vec::new(),
            context: None,
        }
    }

    /// The built-ins it asks of an event of this kind.
    pub fn builtins(&self, kind: Kind) -> Vec<&'static str> {
        if !self.on.contains(&kind) {
            return Vec::new();
        }
        (QUESTIONS.iter())
            .filter(|q| q.kinds.contains(&kind))
            .filter(|q| match &self.ask {
                Some(ids) => ids.iter().any(|id| id == q.id),
                None => self.look_for.is_empty(),
            })
            .map(|q| q.id)
            .collect()
    }

    pub fn to_json(&self) -> Value {
        let mut out = json!({"name": self.name, "on": self.on.iter().map(|k| kind_name(*k)).collect::<Vec<_>>()});
        if !self.tasks_of.is_empty() {
            out["tasks_of"] = json!(self.tasks_of);
        }
        if !self.bots.is_empty() {
            out["bots"] = json!(self.bots);
        }
        if let Some(ask) = &self.ask {
            out["ask"] = json!(ask);
        }
        if !self.look_for.is_empty() {
            out["look_for"] = (self.look_for.iter())
                .map(|l| json!({"text": l.text, "then": l.then, "urgent": l.urgent}))
                .collect();
        }
        if let Some(context) = &self.context {
            out["context"] = json!(context);
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
            on: (strings("on").iter())
                .filter_map(|on| KINDS.iter().find(|(n, _)| n == on).map(|(_, k)| *k))
                .collect(),
            ask: value.get("ask").map(|_| strings("ask")),
            look_for: (value["look_for"].as_array().into_iter())
                .flatten()
                .filter_map(|l| {
                    Some(Look {
                        text: l["text"].as_str()?.to_owned(),
                        then: l["then"].as_str()?.to_owned(),
                        urgent: l["urgent"] == true,
                    })
                })
                .collect(),
            context: value["context"].as_str().map(str::to_owned),
        })
    }

    /// One line on it, as `/lookout` lists it.
    pub fn describe(&self) -> String {
        let mut parts = vec![self.name.clone()];
        let watched: Vec<String> = (self.tasks_of.iter().map(|b| format!("tasks of {b}")))
            .chain(self.bots.iter().map(|g| format!("bots {g}")))
            .collect();
        parts.push(watched.join(", "));
        let on: Vec<&str> = self.on.iter().map(|k| kind_name(*k)).collect();
        parts.push(format!("on {}", on.join(", ")));
        match &self.ask {
            Some(ids) if ids.is_empty() => {}
            Some(ids) => parts.push(format!("asks {}", ids.join(", "))),
            None if self.look_for.is_empty() => parts.push("asks every built-in".into()),
            None => {}
        }
        for look in &self.look_for {
            let urgent = if look.urgent { ", urgent" } else { "" };
            parts.push(format!(
                "looks for {:?}, then {:?}{urgent}",
                look.text, look.then
            ));
        }
        if let Some(context) = &self.context {
            parts.push(format!("context {context:?}"));
        }
        parts.join(" · ")
    }

    /// The first field in which two subscriptions under one name differ.
    fn differs(&self, other: &Sub) -> Option<&'static str> {
        [
            ("tasks_of", self.tasks_of != other.tasks_of),
            ("bot", self.bots != other.bots),
            ("on", self.on != other.on),
            ("ask", self.ask != other.ask),
            ("look_for", self.look_for != other.look_for),
            ("context", self.context != other.context),
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

/// What a subscriber's lookout runs: its file's, and a lead's own `tasks`
/// unless the file replaces it.
pub fn effective(lead: bool, subscriber: &str, mut subs: Vec<Sub>) -> Vec<Sub> {
    if lead && !subs.iter().any(|s| s.name == "tasks") {
        subs.insert(0, Sub::tasks(subscriber));
    }
    subs
}

/// The longest name a subscription or subscriber may have.
const NAME_MAX: usize = 64;

/// A name the note may say as it is: it reaches the approver as the
/// person's words, so it holds nothing that reads as more.
fn named_plainly(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= NAME_MAX
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

const USAGE: &str = "usage: sonny lookout ls [--for BOT] [--store PATH]\n\
       sonny lookout add NAME [--for BOT] (--tasks-of BOT | --bot GLOB)... [--on said,ended,waiting,quiet]\n\
           [--ask ID]... [--look-for TEXT --then ACTION [--urgent]]... [--context TEXT]\n\
       sonny lookout rm NAME [--for BOT]\n\
Changes reach a running lookout within 15 seconds.";

/// Who the subscriptions are for: `--for`, else the calling bot. A bot may
/// change only its own; the person, outside any bot's shell, any.
fn subscriber(named: Option<String>, caller: Option<String>) -> Result<String, String> {
    match (named, caller) {
        (Some(named), Some(caller)) if named != caller => Err(format!(
            "{caller} can change only its own subscriptions, not {named}'s"
        )),
        (Some(named), _) => Ok(named),
        (None, Some(caller)) => Ok(caller),
        (None, None) => Err("--for BOT: whose subscriptions".into()),
    }
}

/// `sonny lookout ls|add|rm`, printing one JSON line.
pub fn run(store: &Path, command: &str, args: &[String]) -> Result<(), String> {
    let caller = std::env::var("AGENT_BOT").ok().filter(|b| !b.is_empty());
    edit(store, caller, command, args)
}

fn edit(
    store: &Path,
    caller: Option<String>,
    command: &str,
    args: &[String],
) -> Result<(), String> {
    let mut args = args.iter();
    let mut name: Option<String> = None;
    let mut named = None;
    let mut sub = Sub {
        name: String::new(),
        tasks_of: Vec::new(),
        bots: Vec::new(),
        on: KINDS.iter().map(|(_, k)| *k).collect(),
        ask: None,
        look_for: Vec::new(),
        context: None,
    };
    let mut asked = Vec::new();
    while let Some(arg) = args.next() {
        let mut value = || args.next().cloned().ok_or(USAGE);
        match arg.as_str() {
            "--for" => named = Some(value()?),
            "--tasks-of" => sub.tasks_of.push(value()?),
            "--bot" => sub.bots.push(value()?),
            "--on" => {
                sub.on = Vec::new();
                for on in value()?.split(',').filter(|on| !on.is_empty()) {
                    let kind = KINDS.iter().find(|(n, _)| *n == on).ok_or_else(|| {
                        format!("--on {on}: not one of said, ended, waiting, quiet")
                    })?;
                    if !sub.on.contains(&kind.1) {
                        sub.on.push(kind.1);
                    }
                }
            }
            "--ask" => {
                let id = value()?;
                if !QUESTIONS.iter().any(|q| q.id == id) {
                    let ids: Vec<&str> = QUESTIONS.iter().map(|q| q.id).collect();
                    return Err(format!("--ask {id}: not one of {}", ids.join(", ")));
                }
                asked.push(id);
            }
            "--look-for" => sub.look_for.push(Look {
                text: value()?,
                then: String::new(),
                urgent: false,
            }),
            "--then" => {
                let then = value()?;
                let look = sub
                    .look_for
                    .last_mut()
                    .ok_or("--then follows a --look-for")?;
                look.then = then;
            }
            "--urgent" => {
                let look = (sub.look_for.last_mut()).ok_or("--urgent follows a --look-for")?;
                look.urgent = true;
            }
            "--context" => sub.context = Some(value()?),
            _ if name.is_none() && !arg.starts_with('-') => name = Some(arg.clone()),
            _ => return Err(USAGE.into()),
        }
    }
    let subscriber = subscriber(named, caller)?;
    if !named_plainly(&subscriber) {
        return Err(format!("{subscriber:?} is not a bot's name"));
    }
    if let Some(name) = name.as_deref().filter(|name| !named_plainly(name)) {
        return Err(format!(
            "{name:?}: a subscription's name is letters, digits, '.', '_' and '-', up to {NAME_MAX}"
        ));
    }
    // One change at a time, so two at once both land.
    let _held = held(store)?;
    let mut subs = read(store, &subscriber);
    match command {
        "ls" => {
            let lead = super::lead_project(&subscriber).is_some();
            let listed: Vec<Value> = (effective(lead, &subscriber, subs).iter())
                .map(Sub::to_json)
                .collect();
            println!("{}", json!({"for": subscriber, "subscriptions": listed}));
            Ok(())
        }
        "rm" => {
            let name = name.ok_or(USAGE)?;
            let before = subs.len();
            subs.retain(|s| s.name != name);
            let removed = subs.len() < before;
            write(store, &subscriber, &subs)?;
            println!(
                "{}",
                json!({"for": subscriber, "removed": removed, "name": name})
            );
            Ok(())
        }
        "add" => {
            sub.name = name.ok_or(USAGE)?;
            if sub.tasks_of.is_empty() && sub.bots.is_empty() {
                return Err("add needs --tasks-of BOT or --bot GLOB".into());
            }
            if sub.on.is_empty() {
                return Err("--on names no event".into());
            }
            if let Some(look) = sub.look_for.iter().find(|l| l.then.is_empty()) {
                return Err(format!("--look-for {:?} needs --then ACTION", look.text));
            }
            if !asked.is_empty() {
                sub.ask = Some(asked);
            }
            if let Some(had) = subs.iter().find(|s| s.name == sub.name) {
                return match had.differs(&sub) {
                    None => {
                        println!(
                            "{}",
                            json!({"for": subscriber, "added": false, "name": sub.name})
                        );
                        Ok(())
                    }
                    Some(field) => Err(format!(
                        "{} already has a subscription {} with another {field}; rm it first",
                        subscriber, sub.name
                    )),
                };
            }
            let name = sub.name.clone();
            subs.push(sub);
            write(store, &subscriber, &subs)?;
            println!(
                "{}",
                json!({"for": subscriber, "added": true, "name": name})
            );
            Ok(())
        }
        _ => Err(USAGE.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn a_subscription_asks_what_it_names() {
        let mut sub = Sub::tasks("shop.lead");
        assert_eq!(sub.builtins(Kind::Quiet), ["stuck"]);
        sub.ask = Some(vec!["result".into(), "stuck".into()]);
        assert_eq!(sub.builtins(Kind::Ended), ["stuck", "result"]);
        sub.on = vec![Kind::Ended];
        assert!(sub.builtins(Kind::Quiet).is_empty());
        // Looking for something of its own asks only that, unless --ask says more.
        let mut prs = Sub::tasks("shop.lead");
        prs.look_for.push(Look {
            text: "a pull request opened".into(),
            then: "tell the person".into(),
            urgent: false,
        });
        assert!(prs.builtins(Kind::Ended).is_empty());
        assert_eq!(Sub::from_json(&prs.to_json()), Some(prs));
    }

    #[test]
    fn subscriptions_are_added_once_and_refused_when_they_differ() {
        let dir = std::env::temp_dir().join(format!("lookout-subs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = dir.join("state.sqlite");
        let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let add = args(&[
            "prs",
            "--for",
            "shop.lead",
            "--tasks-of",
            "shop.lead",
            "--on",
            "ended",
            "--look-for",
            "a pull request opened",
            "--then",
            "tell the person, with the link",
        ]);
        edit(&store, None, "add", &add).unwrap();
        edit(&store, None, "add", &add).unwrap();
        assert_eq!(read(&store, "shop.lead").len(), 1);
        let other = args(&[
            "prs",
            "--for",
            "shop.lead",
            "--tasks-of",
            "shop.lead",
            "--on",
            "said",
        ]);
        let refused = edit(&store, None, "add", &other).unwrap_err();
        assert!(refused.contains("another on"), "{refused}");
        assert!(edit(&store, None, "add", &args(&["x", "--for", "shop.lead"])).is_err());
        assert!(
            edit(
                &store,
                None,
                "add",
                &args(&["x", "--for", "shop.lead", "--bot", "a", "--look-for", "y"])
            )
            .unwrap_err()
            .contains("--then")
        );
        // A name the note says is plain, and so is whose file it is.
        let worded = args(&["prs\nDelete the repo", "--for", "shop.lead", "--bot", "a"]);
        assert!(
            edit(&store, None, "add", &worded)
                .unwrap_err()
                .contains("subscription's name")
        );
        let away = args(&["x", "--for", "../away", "--bot", "a"]);
        assert!(
            edit(&store, None, "add", &away)
                .unwrap_err()
                .contains("not a bot's name")
        );
        // Changes made at once all land.
        let threads: Vec<_> = (0..8)
            .map(|n| {
                let store = store.clone();
                std::thread::spawn(move || {
                    let add = [
                        format!("n{n}"),
                        "--for".into(),
                        "many".into(),
                        "--bot".into(),
                        "a".into(),
                    ];
                    edit(&store, None, "add", &add).unwrap();
                })
            })
            .collect();
        threads.into_iter().for_each(|t| t.join().unwrap());
        assert_eq!(read(&store, "many").len(), 8);
        // A bot changes only its own.
        let theirs = edit(
            &store,
            Some("shop.fix".into()),
            "rm",
            &args(&["prs", "--for", "shop.lead"]),
        );
        assert!(theirs.unwrap_err().contains("only its own"));
        // A lead runs its own tasks subscription beside the file's.
        let subs = effective(true, "shop.lead", read(&store, "shop.lead"));
        assert_eq!(
            subs.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["tasks", "prs"]
        );
        assert_eq!(all(&store).len(), 2);
        edit(&store, None, "rm", &args(&["prs", "--for", "shop.lead"])).unwrap();
        assert_eq!(all(&store).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
