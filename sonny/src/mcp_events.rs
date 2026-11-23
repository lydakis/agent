//! `sonny mcp events NAME`: the events of an MCP server that offers the
//! MCP events extension (SEP-3415), as the lookout source `mcp.NAME`,
//! which the lookout runs as it runs any source's command.
//!
//! What to subscribe to is what the source's follows ask, from the file
//! `$SONNY_SOURCE_WANTS` names: each topic a follow names is an event's
//! name, and the exact values its filter names under `arguments` are what
//! it subscribes with, so a follow of `mcp.docs:comment.created` whose
//! filter is `{"arguments": {"document_id": ["D1"]}}` subscribes to the
//! comments on D1, and hears only those, since each event it prints says
//! the arguments it came by. A follow that names no event asks for every
//! event that needs no arguments. One subscription for each event and
//! arguments, however many follow it; none until a follow asks, and then
//! the server is started.
//!
//! An event its server pushes is streamed (`events/stream`); the others
//! are polled (`events/poll`) as often as the server says, a second
//! apart at least, as the extension advises. Webhooks are not taken:
//! they need an address the server can reach, which a laptop has not.
//! Where each subscription stands, its cursor, is kept in
//! `$SONNY_SOURCE_DIR`, so a restart misses nothing the server still
//! holds; a new one starts from now, and tells nothing of the past.
//!
//! Each event is one line, with no `text`, so a follower reads it whole:
//! `id` (the server's own, and the arguments when there are any, as two
//! subscriptions may share one), `topic` (the event's name), `at`, `data`
//! and `arguments`. A subscription the server refuses is a line on its
//! topic saying why; one that says it skipped events is a line saying so.
//! A connection that breaks or a subscription the server ends stops the
//! command with why, and the lookout runs it again, from where it stood.
use crate::mcp::SOURCE;
use rmcp::model::{ClientRequest, CustomNotification, CustomRequest};
use rmcp::service::{NotificationContext, PeerRequestOptions, RoleClient};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The least time between two polls of a subscription, as the extension
/// advises clients to keep.
const POLL_FLOOR: Duration = Duration::from_secs(1);
/// A stream that said nothing for this many of the heartbeats it agreed
/// to is gone, as the extension says.
const HEARTBEATS: u32 = 3;

/// `sonny mcp events NAME`, as the lookout runs it.
pub fn cli(name: &str) -> Result<(), String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?
        .block_on(run(name))
}

/// One subscription: an event, by name, with its arguments.
#[derive(Clone, Debug, PartialEq)]
struct Asked {
    name: String,
    arguments: Map<String, Value>,
}

impl Asked {
    /// What names it among the others, and keys its cursor: the event's
    /// name, then its arguments, if any, as compact JSON.
    fn key(&self) -> String {
        match self.arguments.is_empty() {
            true => self.name.clone(),
            false => format!("{} {}", self.name, Value::Object(self.arguments.clone())),
        }
    }

    /// A line of its, with its topic and arguments, and `fields`.
    fn line(&self, fields: Value) -> Value {
        let mut line = json!({"topic": self.name});
        if !self.arguments.is_empty() {
            line["arguments"] = Value::Object(self.arguments.clone());
        }
        for (key, value) in fields.as_object().into_iter().flatten() {
            line[key] = value.clone();
        }
        line
    }

    /// An event the server sent for it, as a line: none without an ID.
    fn event(&self, event: &Value) -> Option<Value> {
        let id = event["eventId"].as_str()?;
        let id = match self.arguments.is_empty() {
            true => id.to_owned(),
            false => format!("{id} {}", Value::Object(self.arguments.clone())),
        };
        Some(self.line(json!({"id": id, "at": event["timestamp"], "data": event["data"]})))
    }
}

/// What the follows in `wants` ask of the events `offered`: each
/// subscription once, sorted by its key, and the events they name that
/// the server does not offer.
fn asked(wants: &Value, offered: &[Value]) -> (Vec<Asked>, Vec<String>) {
    let mut asked: BTreeMap<String, Asked> = BTreeMap::new();
    let mut unknown: Vec<String> = Vec::new();
    let offered: HashMap<&str, &Value> = (offered.iter())
        .filter_map(|event| Some((event["name"].as_str()?, event)))
        .collect();
    for follow in wants["follows"].as_array().into_iter().flatten() {
        for arguments in argued(&follow["filter"]) {
            let named: Vec<(String, Map<String, Value>)> = match follow["topics"].as_array() {
                // Each event it names, with the arguments it names.
                Some(topics) => (topics.iter())
                    .filter_map(Value::as_str)
                    .map(|name| (name.to_owned(), arguments.clone()))
                    .collect(),
                // Every event its arguments are enough for, with those of
                // them the event takes.
                None => (offered.iter())
                    .filter_map(|(name, event)| {
                        let schema = &event["inputSchema"];
                        let takes = |key: &str| schema["properties"].get(key).is_some();
                        let given: Map<String, Value> = (arguments.iter())
                            .filter(|(key, _)| takes(key))
                            .map(|(key, value)| (key.clone(), value.clone()))
                            .collect();
                        let required = schema["required"].as_array();
                        let enough = (required.into_iter().flatten())
                            .filter_map(Value::as_str)
                            .all(|key| given.contains_key(key));
                        enough.then(|| ((*name).to_owned(), given))
                    })
                    .collect(),
            };
            for (name, arguments) in named {
                if !offered.contains_key(name.as_str()) {
                    unknown.push(name);
                    continue;
                }
                let one = Asked { name, arguments };
                asked.entry(one.key()).or_insert(one);
            }
        }
    }
    unknown.sort();
    unknown.dedup();
    (asked.into_values().collect(), unknown)
}

/// The arguments a follow's `filter` names: one set for each choice of the
/// exact values it lists under `arguments`, a key it matches otherwise
/// being no argument; one empty set when it names none.
fn argued(filter: &Value) -> Vec<Map<String, Value>> {
    let mut sets = vec![Map::new()];
    for (key, values) in filter["arguments"].as_object().into_iter().flatten() {
        let Some(values) = values.as_array() else {
            continue;
        };
        let plain = |value: &Value| value.is_string() || value.is_number() || value.is_boolean();
        if values.is_empty() || !values.iter().all(plain) {
            continue;
        }
        sets = (sets.iter())
            .flat_map(|set| {
                values.iter().map(move |value| {
                    let mut set = set.clone();
                    set.insert(key.clone(), value.clone());
                    set
                })
            })
            .collect();
    }
    sets
}

/// Where each subscription stands, kept on disk as it moves.
#[derive(Clone)]
struct Cursors {
    file: PathBuf,
    kept: Arc<Mutex<Map<String, Value>>>,
}

impl Cursors {
    fn read(dir: &Path) -> Cursors {
        let file = dir.join("cursors.json");
        let kept = (std::fs::read(&file).ok())
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|kept| kept.as_object().cloned())
            .unwrap_or_default();
        Cursors {
            file,
            kept: Arc::new(Mutex::new(kept)),
        }
    }

    /// Where `asked` stands: none to start from now.
    fn of(&self, asked: &Asked) -> Value {
        let kept = self.kept.lock().unwrap();
        kept.get(&asked.key()).cloned().unwrap_or(Value::Null)
    }

    /// `asked` stands at `cursor` now, once what came before it is
    /// printed: a cursor that is no string leaves it where it was.
    fn set(&self, asked: &Asked, cursor: &Value) -> Result<(), String> {
        if !cursor.is_string() {
            return Ok(());
        }
        let text = {
            let mut kept = self.kept.lock().unwrap();
            if kept.get(&asked.key()) == Some(cursor) {
                return Ok(());
            }
            kept.insert(asked.key(), cursor.clone());
            Value::Object(kept.clone()).to_string()
        };
        let partial = self.file.with_extension("partial");
        std::fs::write(&partial, text)
            .and_then(|()| std::fs::rename(&partial, &self.file))
            .map_err(|e| format!("{}: {e}", self.file.display()))
    }
}

/// Print `line` for the lookout, one event a line.
fn print(line: &Value) {
    println!("{line}");
}

/// A line on `asked`'s topic saying the server skipped events it had.
fn skipped(asked: &Asked) -> Value {
    asked.line(json!({"text": format!(
        "{}: the server says it skipped some of these events; read what it holds now with its \
         tools if that matters",
        asked.name
    )}))
}

/// A line on `asked`'s topic saying the server refused it, and why.
fn refused(server: &str, asked: &Asked, why: &str) -> Value {
    let id = format!("refused {} {why}", asked.key());
    asked.line(json!({"id": id, "text": format!(
        "{SOURCE}{server} refused to subscribe to {}: {why}", asked.key()
    )}))
}

/// Why a request failed: the server's own refusal, which ends only that
/// subscription, or the connection's, which ends them all.
fn failed(error: rmcp::ServiceError) -> Result<String, String> {
    match error {
        rmcp::ServiceError::McpError(error) => Ok(error.message.to_string()),
        other => Err(other.to_string()),
    }
}

/// Hears what the server says unasked, and passes on what the events
/// extension's notifications say.
struct Listener {
    said: tokio::sync::mpsc::UnboundedSender<Heard>,
}

/// A notification of the events extension, and the stream it is of: the
/// request that opened it, from its `_meta`.
type Heard = (CustomNotification, Option<rmcp::model::RequestId>);

impl rmcp::ClientHandler for Listener {
    fn on_custom_notification(
        &self,
        notification: CustomNotification,
        context: NotificationContext<RoleClient>,
    ) -> impl Future<Output = ()> + Send + '_ {
        if notification.method.starts_with("notifications/events/") {
            // rmcp hands the `_meta` with the notification's extensions.
            let id = (context.extensions)
                .get::<rmcp::model::NotificationMetaObject>()
                .and_then(|meta| meta.subscription_id())
                .or_else(|| context.meta.subscription_id());
            let _ = self.said.send((notification, id));
        }
        std::future::ready(())
    }

    fn get_info(&self) -> rmcp::model::ClientConfig {
        crate::mcp::client_info()
    }
}

async fn run(server: &str) -> Result<(), String> {
    let servers = agent_client::config::load()?.sonny.mcp_servers;
    let config = (servers.get(server))
        .filter(|config| config.enabled)
        .ok_or_else(|| format!("no MCP server {server} on"))?;
    let var = |name: &str| std::env::var_os(name).map(PathBuf::from);
    let dir =
        var("SONNY_SOURCE_DIR").ok_or("SONNY_SOURCE_DIR is not set: the lookout runs this")?;
    let wants: Value = (var("SONNY_SOURCE_WANTS"))
        .and_then(|file| std::fs::read(file).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    // Nothing asked, nothing started: the lookout runs it again when a
    // follow asks.
    if wants["follows"].as_array().is_none_or(Vec::is_empty) {
        return std::future::pending().await;
    }
    let (said, mut heard) = tokio::sync::mpsc::unbounded_channel();
    // In this command's process group, so it stops when the lookout
    // stops the command.
    let (client, _group, _stderr) = crate::mcp::open(config, Listener { said }, false).await?;
    let peer = client.peer().clone();
    if !crate::mcp::offers_events(&peer) {
        return Err(format!("{server} offers no events"));
    }
    let offered = crate::mcp::list_events(&peer).await?;
    let (asked, unknown) = asked(&wants, &offered);
    for name in unknown {
        let asked = Asked {
            name,
            arguments: Map::new(),
        };
        print(&refused(server, &asked, "it offers no such event"));
    }
    let cursors = Cursors::read(&dir);
    let delivery = |asked: &Asked| -> Vec<String> {
        let event = offered
            .iter()
            .find(|event| event["name"] == asked.name.as_str());
        (event.and_then(|event| event["delivery"].as_array()))
            .map(|modes| {
                modes
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    // Every stream asked for before a notification is read, so each is
    // known by its subscription when its first one comes.
    let mut polls = tokio::task::JoinSet::new();
    let mut streams: HashMap<String, Stream> = HashMap::new();
    let mut ended = tokio::task::JoinSet::new();
    for one in asked {
        let modes = delivery(&one);
        if modes.iter().any(|mode| mode == "push") {
            let params = json!({"name": one.name, "arguments": one.arguments,
                "cursor": cursors.of(&one)});
            let request = CustomRequest::new("events/stream", Some(crate::mcp::with_meta(params)));
            let handle = peer
                .send_cancellable_request(
                    ClientRequest::CustomRequest(request),
                    PeerRequestOptions::no_options(),
                )
                .await
                .map_err(|e| format!("events/stream: {e}"))?;
            let id = handle.id.to_string();
            let key = one.key();
            ended.spawn(async move { (key, handle.await_response().await) });
            streams.insert(
                id,
                Stream {
                    asked: one,
                    heard: Instant::now(),
                    heartbeat: None,
                },
            );
        } else if modes.iter().any(|mode| mode == "poll") {
            polls.spawn(poll(server.to_owned(), peer.clone(), one, cursors.clone()));
        } else {
            let why = "the server delivers it only by webhook, which Sonny does not take";
            print(&refused(server, &one, why));
        }
    }
    if streams.is_empty() && polls.is_empty() {
        return std::future::pending().await;
    }
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            Some(notification) = heard.recv() => {
                hear(&mut streams, &cursors, notification)?;
            }
            Some(done) = polls.join_next() => {
                done.map_err(|e| e.to_string())??;
            }
            Some(done) = ended.join_next() => {
                let (key, answer) = done.map_err(|e| e.to_string())?;
                if client.is_closed() {
                    return Err(format!("{server}'s connection closed"));
                }
                match answer.map_err(failed) {
                    Ok(_) => return Err(format!("the server ended the stream of {key}")),
                    Err(Ok(why)) => {
                        let asked = (streams.values().map(|stream| &stream.asked))
                            .find(|asked| asked.key() == key)
                            .cloned();
                        streams.retain(|_, stream| stream.asked.key() != key);
                        if let Some(asked) = asked {
                            print(&refused(server, &asked, &why));
                        }
                    }
                    Err(Err(why)) => return Err(format!("the stream of {key}: {why}")),
                }
            }
            _ = tick.tick() => {
                for stream in streams.values() {
                    let Some(beat) = stream.heartbeat else {
                        continue;
                    };
                    if stream.heard.elapsed() > beat * HEARTBEATS {
                        return Err(format!(
                            "the stream of {} said nothing for {}s",
                            stream.asked.key(),
                            stream.heard.elapsed().as_secs()
                        ));
                    }
                }
            }
            else => return Err(format!("{server} has no subscription left")),
        }
    }
}

/// A subscription streamed: what it is, when it was last heard, and how
/// often its server said it would be.
struct Stream {
    asked: Asked,
    heard: Instant,
    heartbeat: Option<Duration>,
}

/// What a stream's notification says: an event printed, a cursor kept, a
/// gap said; the stream ended by its server ends the command.
fn hear(
    streams: &mut HashMap<String, Stream>,
    cursors: &Cursors,
    (notification, id): Heard,
) -> Result<(), String> {
    let Some(stream) = id.and_then(|id| streams.get_mut(&id.to_string())) else {
        return Ok(());
    };
    let method = notification.method.as_str();
    let params = notification.params.unwrap_or_default();
    let params = &params;
    stream.heard = Instant::now();
    let asked = &stream.asked;
    match method.trim_start_matches("notifications/events/") {
        "active" => {
            if params["truncated"] == true {
                print(&skipped(asked));
            }
            stream.heartbeat = params["heartbeatMs"].as_u64().map(Duration::from_millis);
            cursors.set(asked, &params["cursor"])?;
        }
        "event" => {
            if let Some(line) = asked.event(params) {
                print(&line);
            }
            cursors.set(asked, &params["cursor"])?;
        }
        "heartbeat" => cursors.set(asked, &params["cursor"])?,
        "error" => eprintln!("{}: {}", asked.key(), params["error"]),
        "terminated" => {
            return Err(format!(
                "the server ended the stream of {}: {}",
                asked.key(),
                params["error"]["message"]
                    .as_str()
                    .unwrap_or("no reason given")
            ));
        }
        _ => {}
    }
    Ok(())
}

/// Poll `asked` from where it stands, as often as the server says: each
/// event printed, then its cursor kept. The server refusing it is said on
/// its topic and ends it alone.
async fn poll(
    server: String,
    peer: rmcp::Peer<RoleClient>,
    asked: Asked,
    cursors: Cursors,
) -> Result<(), String> {
    loop {
        let params = json!({"name": asked.name, "arguments": asked.arguments,
            "cursor": cursors.of(&asked)});
        let polled = match crate::mcp::ask(&peer, "events/poll", params).await {
            Ok(polled) => polled,
            Err(error) => {
                let why = failed(error).map_err(|why| format!("events/poll: {why}"))?;
                print(&refused(&server, &asked, &why));
                return Ok(());
            }
        };
        if polled["truncated"] == true {
            print(&skipped(&asked));
        }
        for event in polled["events"].as_array().into_iter().flatten() {
            if let Some(line) = asked.event(event) {
                print(&line);
            }
        }
        cursors.set(&asked, &polled["cursor"])?;
        if polled["hasMore"] == true {
            continue;
        }
        let next = (polled["nextPollMs"].as_u64()).map_or(POLL_FLOOR, Duration::from_millis);
        tokio::time::sleep(next.max(POLL_FLOOR)).await;
    }
}

/// Whether `name` is the source an MCP server's events make, and of which.
pub fn server_of(source: &str) -> Option<&str> {
    source.strip_prefix(SOURCE)
}

/// What a `follows` answer that shows `mcp.NAME` whole adds: the events
/// its server offers, each with what it takes, and how to follow one.
pub fn say_events(said: &mut Value) {
    let Some(server) = said["source"]["name"].as_str().and_then(server_of) else {
        return;
    };
    let Ok(config) = crate::kept::config() else {
        return;
    };
    let Some(config) = config.sonny.mcp_servers.get(server) else {
        return;
    };
    let known = (crate::mcp::cache_dir())
        .and_then(|cache| crate::mcp::kept(&cache, server, config))
        .unwrap_or_default();
    let offers: Vec<Value> = (known.events.iter())
        .map(|event| {
            json!({"name": event["name"], "description": event["description"],
                "arguments": event["inputSchema"], "data": event["payloadSchema"]})
        })
        .collect();
    said["source"]["offers"] = json!(offers);
    said["offers"] = json!(OFFERS);
}

/// What `offers` means, as a `follows` answer says.
pub const OFFERS: &str = "An MCP server's events: follow one as SOURCE:EVENT, and give \
    the arguments it takes as exact values in the filter, as {\"arguments\": {\"KEY\": \
    [\"VALUE\"]}}; a follow of the source alone hears every event that needs no arguments. \
    Its events say the arguments they came by, and their payload under data.";

#[cfg(test)]
mod tests {
    use super::*;

    fn offered() -> Vec<Value> {
        vec![
            json!({"name": "ci.failed", "delivery": ["poll"],
                "inputSchema": {"type": "object", "properties": {"repo": {"type": "string"}}}}),
            json!({"name": "comment.created", "delivery": ["push", "poll"],
                "inputSchema": {"type": "object", "properties": {"document_id": {"type": "string"}},
                    "required": ["document_id"]}}),
        ]
    }

    #[test]
    fn a_follow_s_topics_are_the_events_and_its_filter_s_arguments_what_it_subscribes_with() {
        let wants = json!({"follows": [
            {"topics": ["comment.created"],
             "filter": {"arguments": {"document_id": ["D1", "D2"]}, "data": {"by": ["ana"]}}},
            // A second follow of the same is one subscription.
            {"topics": ["comment.created"], "filter": {"arguments": {"document_id": ["D1"]}}},
            {"topics": ["nope"], "filter": null},
        ]});
        let (asked, unknown) = asked(&wants, &offered());
        let keys: Vec<String> = asked.iter().map(Asked::key).collect();
        assert_eq!(
            keys,
            [
                r#"comment.created {"document_id":"D1"}"#,
                r#"comment.created {"document_id":"D2"}"#
            ]
        );
        assert_eq!(unknown, ["nope"]);
    }

    #[test]
    fn a_follow_of_the_source_alone_asks_every_event_its_arguments_are_enough_for() {
        let bare = json!({"follows": [{"topics": null, "filter": null}]});
        let keys: Vec<String> = asked(&bare, &offered()).0.iter().map(Asked::key).collect();
        assert_eq!(keys, ["ci.failed"]);
        // Arguments go to the events that take them.
        let argued = json!({"follows": [{"topics": null,
            "filter": {"arguments": {"document_id": ["D1"], "repo": ["web"]}}}]});
        let keys: Vec<String> = asked(&argued, &offered())
            .0
            .iter()
            .map(Asked::key)
            .collect();
        assert_eq!(
            keys,
            [
                r#"ci.failed {"repo":"web"}"#,
                r#"comment.created {"document_id":"D1"}"#
            ]
        );
    }

    #[test]
    fn a_filter_s_arguments_are_only_its_exact_values() {
        let sets = argued(&json!({"arguments": {"a": [1, 2], "b": [{"prefix": "x"}], "c": []}}));
        assert_eq!(
            sets,
            [
                json!({"a": 1}).as_object().unwrap().clone(),
                json!({"a": 2}).as_object().unwrap().clone()
            ]
        );
        assert_eq!(argued(&Value::Null), [Map::new()]);
    }

    #[test]
    fn an_event_is_a_line_with_no_text_its_id_scoped_to_its_arguments() {
        let plain = Asked {
            name: "ci.failed".into(),
            arguments: Map::new(),
        };
        let event = json!({"eventId": "e1", "name": "ci.failed", "timestamp": "2026-11-23T03:00:00Z",
            "data": {"run": 7}, "cursor": "c1"});
        assert_eq!(
            plain.event(&event).unwrap(),
            json!({"id": "e1", "topic": "ci.failed", "at": "2026-11-23T03:00:00Z", "data": {"run": 7}})
        );
        let argued = Asked {
            name: "ci.failed".into(),
            arguments: json!({"repo": "web"}).as_object().unwrap().clone(),
        };
        let line = argued.event(&event).unwrap();
        assert_eq!(line["id"], r#"e1 {"repo":"web"}"#);
        assert_eq!(line["arguments"], json!({"repo": "web"}));
        assert!(line.get("text").is_none());
        // One without an ID is no event.
        assert!(plain.event(&json!({"data": {}})).is_none());
    }

    #[test]
    fn a_cursor_is_kept_on_disk_and_read_again() {
        let dir = std::env::temp_dir().join(format!("mcp-events-cursors-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let asked = Asked {
            name: "ci.failed".into(),
            arguments: Map::new(),
        };
        let cursors = Cursors::read(&dir);
        assert_eq!(cursors.of(&asked), Value::Null);
        cursors.set(&asked, &json!("c1")).unwrap();
        // A cursor that is not one leaves it where it was.
        cursors.set(&asked, &Value::Null).unwrap();
        assert_eq!(Cursors::read(&dir).of(&asked), json!("c1"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_poll_s_result_reads_whole_whichever_result_it_is_taken_for() {
        let polled = json!({"events": [{"eventId": "e1", "name": "ci.failed",
            "timestamp": "t", "data": {"run": 7}}], "cursor": "c2", "truncated": false,
            "hasMore": false, "nextPollMs": 5000});
        let read: rmcp::model::ServerResult = serde_json::from_value(polled.clone()).unwrap();
        assert_eq!(crate::mcp::result(read).unwrap(), polled);
        let listed = json!({"events": [{"name": "ci.failed", "description": "d",
            "delivery": ["poll"], "inputSchema": {"type": "object"}, "payloadSchema": {}}]});
        let read: rmcp::model::ServerResult = serde_json::from_value(listed.clone()).unwrap();
        assert_eq!(crate::mcp::result(read).unwrap(), listed);
    }
}
