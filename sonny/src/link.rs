//! Links: this machine's browser, MCP servers and opens, used by the agents
//! of a host over SSH, as `[sonny.links.NAME]` says. The linked machine
//! dials: its tool host keeps `ssh HOST sonny link serve MACHINE` running
//! and answers what comes down it, so the host needs no port, no
//! forwarding and no setting of its sshd, and nothing of the link is left
//! there when the machine sleeps.
//!
//! There, `sonny link serve` listens on `~/.agent/links/MACHINE.sock` for
//! that machine's tool host and window. An agent's sandbox reaches Unix
//! sockets once it reaches the network, so the socket alone proves
//! nothing: each connection first writes a file the server names into
//! `~/.agent/links/.proof`, which no sandboxed command writes, so only
//! the person's own programs send calls, and every call goes through its
//! agent's gate there. What a call asks to confirm comes back up the link
//! to that gate.
//!
//! Each side speaks JSON, one object a line. Down the SSH session, each
//! call carries the number the server gave it.
use crate::exec::Steps;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

/// What the two ends of a link speak: one that speaks another is refused,
/// so both run a Sonny built from the same commit, or near it.
pub const PROTOCOL: u64 = 1;

/// How long a machine has to take a call before it counts as not
/// connected, and a connection to prove itself.
const ACK: Duration = Duration::from_secs(3);
/// How often the lender says it is there, and how long the other end
/// waits without a word before it lets go: a machine asleep never says so.
const PING: Duration = Duration::from_secs(15);
const SILENT: Duration = Duration::from_secs(45);
/// The waits between dials, doubled from the first while each fails soon,
/// started over after a link that held.
const BACKOFF: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
const HEALTHY: Duration = Duration::from_secs(60);
/// The largest line either end reads: a file opened there, as base64.
const LINE_BYTES: usize = 48 << 20;
/// The largest file opened on another machine.
const OPEN_BYTES: usize = 32 << 20;
/// The files opened on another machine: what a viewer shows, never what
/// runs.
const VIEWED: [&str; 14] = [
    "png", "jpg", "jpeg", "gif", "webp", "heic", "bmp", "tiff", "pdf", "txt", "md", "csv", "log",
    "json",
];

/// The codes a call's error may carry back, as typed here.
const CODES: [&str; 10] = [
    "browser_refused",
    "desktop_refused",
    "exec_refused",
    "open_refused",
    "timed_out",
    "not_connected",
    "no_machine",
    "link_refused",
    "caller_unknown",
    "escalation_refused",
];

type Failed = (&'static str, String);

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Where the machines linked here keep their sockets and what they lend.
fn here() -> Option<PathBuf> {
    Some(home()?.join(".agent/links"))
}

/// Where this machine's own links say how they stand.
fn mine() -> Option<PathBuf> {
    Some(home()?.join(".agent/sonny/links"))
}

/// `dir`, made if need be, that only the person reads.
fn private(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Writes `text` to `path` whole, through a file beside it.
fn replace(path: &Path, text: &str) -> std::io::Result<()> {
    let partial = path.with_extension(format!("{}.partial", std::process::id()));
    std::fs::write(&partial, text)?;
    std::fs::rename(&partial, path)
}

/// A machine linked here, as it last said what it lends.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Machine {
    pub name: String,
    /// Whether its browser runs commands sent here.
    pub browser: bool,
    /// Whether that browser starts signed in as the person is there.
    pub signed_in: bool,
    /// Whether its desktop runs commands sent here.
    pub desktop: bool,
    /// Whether it opens links and files sent to it.
    pub open: bool,
    /// Its MCP servers, by name.
    pub servers: BTreeMap<String, Lent>,
}

/// An MCP server of another machine: what it says of itself, and its tools
/// as it last listed them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Lent {
    pub description: Option<String>,
    pub tools: Vec<Value>,
}

impl Machine {
    fn of(name: &str, said: &Value) -> Machine {
        let servers = (said["servers"].as_object().into_iter().flatten())
            .map(|(server, lent)| {
                let lent = Lent {
                    description: lent["description"].as_str().map(str::to_owned),
                    tools: lent["tools"].as_array().cloned().unwrap_or_default(),
                };
                (server.clone(), lent)
            })
            .collect();
        Machine {
            name: name.to_owned(),
            browser: said["browser"] == true,
            signed_in: said["signed_in"] == true,
            desktop: said["desktop"] == true,
            open: said["open"] == true,
            servers,
        }
    }
}

/// The machines linked here, by name: each stays, with what it lends, while
/// it is away, so its agents' tools stay as they are.
pub fn machines() -> Vec<Machine> {
    let Some(dir) = here() else {
        return Vec::new();
    };
    let mut machines: Vec<Machine> = (std::fs::read_dir(dir).into_iter().flatten().flatten())
        .filter_map(|entry| {
            let name = entry
                .file_name()
                .to_str()?
                .strip_suffix(".json")?
                .to_owned();
            agent_client::config::named(&name).then_some(())?;
            let said: Value = serde_json::from_slice(&std::fs::read(entry.path()).ok()?).ok()?;
            Some(Machine::of(&name, &said))
        })
        .collect();
    machines.sort_by(|a, b| a.name.cmp(&b.name));
    machines
}

/// What the machines linked here lend, as one stamp: it changes when one
/// comes, goes or lends otherwise.
pub fn stamp() -> Vec<(String, std::time::SystemTime)> {
    let Some(dir) = here() else {
        return Vec::new();
    };
    let mut stamp: Vec<(String, std::time::SystemTime)> =
        (std::fs::read_dir(dir).into_iter().flatten().flatten())
            .filter_map(|entry| {
                let name = entry.file_name().to_str()?.to_owned();
                name.ends_with(".json").then_some(())?;
                Some((name, entry.metadata().ok()?.modified().ok()?))
            })
            .collect();
    stamp.sort();
    stamp
}

/// When `machine` last said a word here, in milliseconds.
fn seen(machine: &str) -> Option<u64> {
    let text = std::fs::read_to_string(here()?.join(format!("{machine}.seen"))).ok()?;
    text.trim().parse().ok()
}

/// How long ago `ms` was, in a few words.
fn ago(ms: u64) -> String {
    let secs = now_ms().saturating_sub(ms) / 1000;
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", secs / 60),
        3600..86400 => format!("{} h ago", secs / 3600),
        _ => format!("{} days ago", secs / 86400),
    }
}

fn not_connected(machine: &str) -> Failed {
    let last = match seen(machine) {
        Some(ms) => format!("; it was last connected {}", ago(ms)),
        None => String::new(),
    };
    ("not_connected", format!("{machine} is not connected{last}"))
}

/// A line of JSON from `reader`, at most `max` bytes; none at its end.
/// `heard`, when given, is set to when the last of its bytes came, so a
/// line that takes long to come still says the other end is there.
async fn line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max: usize,
    heard: Option<&AtomicU64>,
) -> std::io::Result<Option<Value>> {
    let mut read = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if let Some(heard) = heard {
            heard.store(now_ms(), Ordering::Relaxed);
        }
        if available.is_empty() {
            return match read.is_empty() {
                true => Ok(None),
                false => Err(std::io::ErrorKind::UnexpectedEof.into()),
            };
        }
        let (taken, ended) = match available.iter().position(|&b| b == b'\n') {
            Some(n) => (n + 1, true),
            None => (available.len(), false),
        };
        read.extend_from_slice(&available[..taken]);
        reader.consume(taken);
        if read.len() > max {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "a line too long",
            ));
        }
        if ended {
            let value = serde_json::from_slice(&read)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            return Ok(Some(value));
        }
    }
}

/// The lines of JSON from `reader`, each at most `max` bytes, read by a
/// task of their own: a line half read is never lost to a `select!` that
/// took another branch. They end at the reader's end or its first fault.
fn lines<R: AsyncBufRead + Unpin + Send + 'static>(
    mut reader: R,
    max: usize,
    heard: Option<Arc<AtomicU64>>,
) -> mpsc::Receiver<std::io::Result<Value>> {
    let (tx, rx) = mpsc::channel(4);
    tokio::spawn(async move {
        loop {
            let read = line(&mut reader, max, heard.as_deref()).await;
            let ended = !matches!(read, Ok(Some(_)));
            let sent = match read {
                Ok(Some(value)) => tx.send(Ok(value)).await,
                Ok(None) => return,
                Err(e) => tx.send(Err(e)).await,
            };
            if ended || sent.is_err() {
                return;
            }
        }
    });
    rx
}

async fn send<W: AsyncWrite + Unpin>(writer: &mut W, value: &Value) -> std::io::Result<()> {
    let mut text = value.to_string();
    text.push('\n');
    writer.write_all(text.as_bytes()).await?;
    writer.flush().await
}

/// 32 hex digits no one can guess.
fn nonce() -> String {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    if let Ok(mut random) = std::fs::File::open("/dev/urandom") {
        let _ = random.read_exact(&mut bytes);
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A call's end as the link carries it.
fn done(answer: Result<Value, Failed>) -> Value {
    match answer {
        Ok(output) => json!({"output": output}),
        Err((code, detail)) => json!({"error": {"code": code, "detail": detail}}),
    }
}

/// A call's end as the link carried it.
fn ended(done: &Value) -> Result<Value, Failed> {
    match done.get("error") {
        None => Ok(done["output"].clone()),
        Some(error) => {
            let said = error["code"].as_str().unwrap_or_default();
            let code = CODES
                .iter()
                .find(|&&c| c == said)
                .copied()
                .unwrap_or("link_refused");
            Err((
                code,
                error["detail"].as_str().unwrap_or_default().to_owned(),
            ))
        }
    }
}

/// Call `tool` of `machine` with `payload`, putting what it asks to
/// confirm to `steps`: its answer, as that machine gave it.
pub async fn call(
    machine: &str,
    tool: &str,
    payload: Value,
    steps: Option<&Steps>,
) -> Result<Value, Failed> {
    let dir = here().ok_or(("no_machine", "HOME is not set".to_owned()))?;
    if !agent_client::config::named(machine) || !dir.join(format!("{machine}.json")).exists() {
        let linked: Vec<String> = machines().into_iter().map(|m| m.name).collect();
        return Err((
            "no_machine",
            match linked.is_empty() {
                true => format!("no machine {machine} is linked here"),
                false => format!(
                    "no machine {machine} is linked here, only {}",
                    linked.join(", ")
                ),
            },
        ));
    }
    let socket = dir.join(format!("{machine}.sock"));
    let stream =
        (tokio::net::UnixStream::connect(&socket).await).map_err(|_| not_connected(machine))?;
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    let gone = |_| not_connected(machine);
    // Proved: a file only the person's programs write.
    let challenge = tokio::time::timeout(ACK, line(&mut read, 1024, None)).await;
    let challenge = challenge
        .map_err(|_| not_connected(machine))?
        .map_err(gone)?;
    let challenge = challenge.as_ref().and_then(|c| c["challenge"].as_str());
    let challenge = challenge
        .filter(|c| c.len() == 32 && c.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| not_connected(machine))?
        .to_owned();
    let proofs = dir.join(".proof");
    private(&proofs).map_err(|e| ("link_refused", e.to_string()))?;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(proofs.join(&challenge))
        .map_err(|e| ("link_refused", format!("{}: {e}", proofs.display())))?;
    send(&mut write, &json!({"proof": challenge}))
        .await
        .map_err(gone)?;
    let called = json!({"call": {"tool": tool, "payload": payload}});
    send(&mut write, &called).await.map_err(gone)?;
    // Queued at once while the machine is there, however long the call
    // then takes to carry, and behind whatever else is on its way.
    let first = tokio::time::timeout(ACK, line(&mut read, LINE_BYTES, None)).await;
    let mut next = Some(first.map_err(|_| not_connected(machine))?.map_err(gone)?);
    loop {
        let said = match next.take() {
            Some(said) => said,
            None => line(&mut read, LINE_BYTES, None).await.map_err(gone)?,
        };
        let Some(said) = said else {
            return Err(("link_refused", format!("{machine} dropped the call")));
        };
        if said.get("done").is_some() {
            return ended(&said["done"]);
        }
        if let Some(asked) = said.get("ask") {
            let verdict = match steps {
                Some(steps) => {
                    let tool = asked["tool"].as_str().unwrap_or_default();
                    let called = asked["called"].as_str().unwrap_or(tool);
                    steps.ask(tool, called, asked["arguments"].clone()).await
                }
                None => Err(format!("{tool} asks no approval")),
            };
            let verdict = match verdict {
                Ok(step) => json!({"allowed": step}),
                Err(why) => json!({"refused": why}),
            };
            let answer = json!({"verdict": verdict, "seq": said["seq"]});
            send(&mut write, &answer).await.map_err(gone)?;
        }
        if let (Some(ran), Some(steps)) = (said.get("ran"), steps)
            && let Some(step) = ran["step"].as_str()
        {
            steps.ran(step.to_owned(), ran["failed"] == true).await;
        }
    }
}

/// Open `target`, a web address or a file here, on a machine linked here
/// that is connected: the machine's name, or why none did.
pub fn open(target: &str) -> Result<String, String> {
    // Those heard from lately, each tried in turn while one is not there.
    let connected: Vec<Machine> = (machines().into_iter())
        .filter(|machine| machine.open)
        .filter(|machine| {
            seen(&machine.name)
                .is_some_and(|ms| now_ms().saturating_sub(ms) < SILENT.as_millis() as u64)
        })
        .collect();
    if connected.is_empty() {
        return Err("no machine linked here is connected".into());
    }
    let payload = match target.starts_with("https://") || target.starts_with("http://") {
        true => json!({"url": target}),
        false => {
            let path = Path::new(target.strip_prefix("file://").unwrap_or(target));
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or("not a file")?;
            if !viewed(name) {
                return Err(format!("{name} is not a file a viewer opens"));
            }
            // At most one byte more than may go, however large the file.
            use std::io::Read;
            let mut bytes = Vec::new();
            (std::fs::File::open(path).map_err(|e| e.to_string())?)
                .take(OPEN_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| e.to_string())?;
            if bytes.len() > OPEN_BYTES {
                return Err(format!("{name} is more than {} MB", OPEN_BYTES >> 20));
            }
            use base64::Engine;
            let data = base64::engine::general_purpose::STANDARD.encode(bytes);
            json!({"file": {"name": name, "data": data}})
        }
    };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(e) => {
                let _ = tx.send(Err(e.to_string()));
                return;
            }
        };
        let mut why = String::new();
        for machine in connected {
            // Which it is going to, for a window that stops waiting.
            let _ = tx.send(Ok(Err(machine.name.clone())));
            match runtime.block_on(call(&machine.name, "open", payload.clone(), None)) {
                Ok(_) => {
                    let _ = tx.send(Ok(Ok(machine.name)));
                    return;
                }
                Err(("not_connected", detail)) => why = detail,
                Err((_, detail)) => {
                    let _ = tx.send(Err(detail));
                    return;
                }
            }
        }
        let _ = tx.send(Err(why));
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut going = String::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(Ok(Ok(name))) => return Ok(name),
            Ok(Ok(Err(name))) => going = name,
            Ok(Err(error)) => return Err(error),
            // Still on its way, which a large file over a slow link may be:
            // it opens when it gets there, and this window does not wait.
            Err(_) => {
                return Err(format!(
                    "still sending it to {going}; it opens there when it arrives"
                ));
            }
        }
    }
}

fn viewed(name: &str) -> bool {
    let extension = name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase());
    extension.is_some_and(|ext| VIEWED.contains(&ext.as_str()))
}

// ---- `sonny link serve`: the end on the machine that borrows ----

/// The calls in flight down the link, by number, each to its connection.
type Waiting = Arc<Mutex<HashMap<u64, mpsc::UnboundedSender<Value>>>>;

/// How the lender stands: unknown until it says hello, then its protocol
/// matched or why not.
type Ready = Arc<Mutex<Option<Result<(), String>>>>;

/// `sonny link serve MACHINE`, run by the lender's SSH session: listen for
/// this machine's calls to `machine` until the session ends or goes quiet.
async fn serve(machine: String) -> Result<(), String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    if !agent_client::config::named(&machine) {
        return Err(format!(
            "{machine}: a machine is 1 to 64 letters, digits, _ or -"
        ));
    }
    let dir = here().ok_or("HOME is not set")?;
    private(&dir).map_err(|e| e.to_string())?;
    private(&dir.join(".proof")).map_err(|e| e.to_string())?;
    let path = dir.join(format!("{machine}.sock"));
    // One left by a session that ended unseen.
    let _ = std::fs::remove_file(&path);
    let listener = tokio::net::UnixListener::bind(&path).map_err(|e| e.to_string())?;
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    let ours = std::fs::metadata(&path).map(|m| m.ino()).ok();
    let (out, mut outgoing) = mpsc::unbounded_channel::<Value>();
    tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(value) = outgoing.recv().await {
            if send(&mut stdout, &value).await.is_err() {
                return;
            }
        }
    });
    let _ = out.send(json!({"hello": {"protocol": PROTOCOL}}));
    let waiting: Waiting = Arc::default();
    let ready: Ready = Arc::default();
    let next = Arc::new(AtomicU64::new(1));
    // When the lender last sent a byte: a ping a `PING`, or a line long
    // in coming.
    let heard = Arc::new(AtomicU64::new(now_ms()));
    let mut stdin = lines(
        BufReader::new(tokio::io::stdin()),
        LINE_BYTES,
        Some(heard.clone()),
    );
    let mut told = Instant::now() - PING;
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            said = stdin.recv() => {
                let Some(Ok(said)) = said else { break };
                if told.elapsed() >= Duration::from_secs(10) {
                    told = Instant::now();
                    let _ = replace(&dir.join(format!("{machine}.seen")), &now_ms().to_string());
                }
                if let Some(hello) = said.get("hello") {
                    let matched = match hello["protocol"].as_u64() {
                        Some(PROTOCOL) => Ok(()),
                        other => Err(format!(
                            "{machine} speaks link protocol {}, this machine {PROTOCOL}: run the same Sonny on both",
                            other.map_or("none".into(), |p| p.to_string())
                        )),
                    };
                    if matched.is_ok() {
                        let lent = hello["lends"].to_string();
                        let file = dir.join(format!("{machine}.json"));
                        if std::fs::read_to_string(&file).ok().as_deref() != Some(lent.as_str()) {
                            let _ = replace(&file, &lent);
                        }
                    }
                    *ready.lock().unwrap() = Some(matched);
                    continue;
                }
                let Some(id) = said["id"].as_u64() else { continue };
                let to = waiting.lock().unwrap().get(&id).cloned();
                if let Some(to) = to {
                    let mut said = said;
                    said.as_object_mut().map(|said| said.remove("id"));
                    let _ = to.send(said);
                }
            }
            accepted = listener.accept() => {
                if let Ok((stream, _)) = accepted {
                    let (out, waiting, ready, next, dir, heard, machine) = (
                        out.clone(), waiting.clone(), ready.clone(), next.clone(), dir.clone(),
                        heard.clone(), machine.clone(),
                    );
                    tokio::spawn(async move {
                        let link = Session { machine: &machine, dir: &dir, out, waiting, ready, next, heard };
                        let _ = borrow(stream, link).await;
                    });
                }
            }
            _ = tick.tick() => {
                // Quiet too long, as a lender asleep; or another session's
                // socket in this one's place.
                let replaced = std::fs::metadata(&path).map(|m| m.ino()).ok() != ours;
                let quiet = now_ms().saturating_sub(heard.load(Ordering::Relaxed));
                if quiet >= SILENT.as_millis() as u64 || replaced {
                    break;
                }
            }
        }
    }
    let _ = replace(&dir.join(format!("{machine}.seen")), &now_ms().to_string());
    if std::fs::metadata(&path).map(|m| m.ino()).ok() == ours {
        let _ = std::fs::remove_file(&path);
    }
    Ok(())
}

/// The session, as a connection to it sees it.
struct Session<'a> {
    machine: &'a str,
    dir: &'a Path,
    out: mpsc::UnboundedSender<Value>,
    waiting: Waiting,
    ready: Ready,
    next: Arc<AtomicU64>,
    heard: Arc<AtomicU64>,
}

/// One connection from this machine's own programs: proved, then its call
/// sent down the link and what comes back sent to it, until it ends.
async fn borrow(stream: tokio::net::UnixStream, link: Session<'_>) -> std::io::Result<()> {
    let Session {
        machine,
        dir,
        out,
        waiting,
        ready,
        next,
        heard,
    } = link;
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    let challenge = nonce();
    send(&mut write, &json!({"challenge": challenge})).await?;
    let proof = tokio::time::timeout(ACK, line(&mut read, 1024, None)).await;
    let proved = matches!(&proof, Ok(Ok(Some(said))) if said["proof"] == challenge.as_str())
        && proved(&dir.join(".proof").join(&challenge));
    if !proved {
        return Ok(());
    }
    let Some(called) = tokio::time::timeout(ACK, line(&mut read, LINE_BYTES, None))
        .await
        .ok()
        .and_then(Result::ok)
        .flatten()
    else {
        return Ok(());
    };
    let standing = ready.lock().unwrap().clone();
    if let Some(Err(why)) = standing {
        return send(
            &mut write,
            &json!({"done": done(Err(("link_refused", why)))}),
        )
        .await;
    }
    // Not there: nothing heard from it in longer than it goes between
    // pings, and a little.
    let quiet = now_ms().saturating_sub(heard.load(Ordering::Relaxed));
    if quiet > (PING + ACK).as_millis() as u64 {
        let gone = done(Err(not_connected(machine)));
        return send(&mut write, &json!({"done": gone})).await;
    }
    let id = next.fetch_add(1, Ordering::SeqCst);
    let mut read = lines(read, LINE_BYTES, None);
    let (to, mut from) = mpsc::unbounded_channel();
    waiting.lock().unwrap().insert(id, to);
    let _ = out.send(json!({"id": id, "call": called["call"]}));
    send(&mut write, &json!({"queued": true})).await?;
    let ended = loop {
        tokio::select! {
            said = from.recv() => {
                let Some(said) = said else { break false };
                let finished = said.get("done").is_some();
                if send(&mut write, &said).await.is_err() {
                    break false;
                }
                if finished {
                    break true;
                }
            }
            said = read.recv() => {
                match said {
                    Some(Ok(mut said)) => {
                        said["id"] = json!(id);
                        let _ = out.send(said);
                    }
                    _ => break false,
                }
            }
        }
    };
    waiting.lock().unwrap().remove(&id);
    if !ended {
        let _ = out.send(json!({"id": id, "cancel": true}));
    }
    Ok(())
}

/// Whether the proof file is there, a file of this user's, taken away as
/// it is read.
fn proved(file: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid takes nothing and cannot fail.
    let me = unsafe { libc::geteuid() };
    let found =
        std::fs::symlink_metadata(file).is_ok_and(|meta| meta.is_file() && meta.uid() == me);
    let _ = std::fs::remove_file(file);
    found
}

// ---- the lender: this machine's tool host, dialing ----

/// What the lender's answer to a call waits on: its verdicts, by call and
/// question.
type Verdicts = Arc<Mutex<HashMap<(u64, u64), oneshot::Sender<Value>>>>;

/// The steps of a call made over a link: each question goes back up it, to
/// the gate of the agent that made the call.
pub struct Asker {
    id: u64,
    out: mpsc::UnboundedSender<Value>,
    verdicts: Verdicts,
    seq: AtomicU64,
}

impl Asker {
    pub async fn ask(
        &self,
        tool: &str,
        called: &str,
        arguments: Value,
    ) -> Result<Option<String>, String> {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let (tx, rx) = oneshot::channel();
        self.verdicts.lock().unwrap().insert((self.id, seq), tx);
        let asked = json!({"id": self.id, "seq": seq,
            "ask": {"tool": tool, "called": called, "arguments": arguments}});
        let _ = self.out.send(asked);
        let verdict = rx.await;
        self.verdicts.lock().unwrap().remove(&(self.id, seq));
        let verdict =
            verdict.map_err(|_| format!("{called} could not be approved: the link closed"))?;
        match verdict.get("refused") {
            Some(why) => Err(why.as_str().unwrap_or("denied").to_owned()),
            None => Ok(verdict["allowed"].as_str().map(str::to_owned)),
        }
    }

    pub async fn ran(&self, step: String, failed: bool) {
        let _ = (self.out).send(json!({"id": self.id, "ran": {"step": step, "failed": failed}}));
    }
}

/// Keep this machine's links up while its tool host runs, each dialed
/// again as it drops, and the links the config adds or takes away dialed
/// or let go within seconds.
pub async fn keep(hub: crate::mcp::Hub) {
    let mut running: HashMap<String, (agent_client::config::Link, tokio::task::JoinHandle<()>)> =
        HashMap::new();
    loop {
        // A config that does not load, say while it is being edited, keeps
        // the links as they are; `sonny link` says what is wrong with it.
        let Ok(config) = agent_client::config::load() else {
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        };
        let links = config.sonny.links;
        running.retain(|key, (link, job)| {
            let kept = links.get(key) == Some(link) && !job.is_finished();
            if !kept {
                job.abort();
            }
            kept
        });
        for (key, link) in links {
            if let std::collections::hash_map::Entry::Vacant(slot) = running.entry(key.clone()) {
                let (job_link, hub) = (link.clone(), hub.clone());
                let job = tokio::spawn(async move { redial(key, job_link, hub).await });
                slot.insert((link, job));
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Dial `link` over and over, waiting longer each time it fails soon:
/// once this is the one tool host of this home that holds it, since the
/// host keeps one session of each machine, and a second would put the
/// first out.
async fn redial(key: String, link: agent_client::config::Link, hub: crate::mcp::Hub) {
    let _held = loop {
        let lock = format!("{key}.lock");
        let held = tokio::task::spawn_blocking(move || {
            let dir = mine().filter(|dir| private(dir).is_ok())?;
            crate::lookout::hold(&dir.join(lock)).ok().flatten()
        });
        match held.await {
            Ok(Some(file)) => break file,
            _ => tokio::time::sleep(Duration::from_secs(5)).await,
        }
    };
    let mut wait = BACKOFF;
    loop {
        let started = Instant::now();
        let ended = dial(&key, &link, &hub).await;
        stood(&key, &link, false, ended.err());
        if started.elapsed() >= HEALTHY {
            wait = BACKOFF;
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(BACKOFF_MAX);
    }
}

/// Say how `key` stands, for `sonny link`.
fn stood(key: &str, link: &agent_client::config::Link, connected: bool, error: Option<String>) {
    let Some(dir) = mine() else {
        return;
    };
    if private(&dir).is_err() {
        return;
    }
    let said = json!({"host": link.host, "machine": machine_of(link), "connected": connected,
        "at": now_ms(), "error": error});
    let _ = replace(&dir.join(format!("{key}.json")), &said.to_string());
}

/// What the agents over `link` call this machine: its own `machine`, else
/// this machine's hostname, as `laptop` of `Laptop.local`.
pub fn machine_of(link: &agent_client::config::Link) -> String {
    match link.machine.is_empty() {
        true => this_machine(),
        false => link.machine.clone(),
    }
}

/// This machine's hostname, made a name: its first word, lowercase.
fn this_machine() -> String {
    let mut bytes = [0u8; 256];
    // SAFETY: the buffer is as long as said, and gethostname writes within it.
    let read = unsafe { libc::gethostname(bytes.as_mut_ptr().cast(), bytes.len()) };
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let host = match read {
        0 => String::from_utf8_lossy(&bytes[..end]).into_owned(),
        _ => String::new(),
    };
    name_of(&host.to_lowercase())
}

/// The words of a remote command, each quoted for the shell there.
fn quoted(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// The SSH session of one dial: options that keep it to itself and notice a
/// network gone within a minute.
fn ssh(link: &agent_client::config::Link) -> tokio::process::Command {
    let program = std::env::var_os("SONNY_LINK_SSH").unwrap_or_else(|| "ssh".into());
    let mut command = tokio::process::Command::new(program);
    command.args([
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=3",
        "-o",
        "ConnectTimeout=20",
        "-o",
        "ControlMaster=no",
        "-o",
        "ControlPath=none",
        "-o",
        "ForwardAgent=no",
        "--",
        &link.host,
    ]);
    command
}

/// One SSH session to `link`: what comes down it answered, until it ends.
async fn dial(
    key: &str,
    link: &agent_client::config::Link,
    hub: &crate::mcp::Hub,
) -> Result<(), String> {
    let sonny = link.command.as_deref().unwrap_or("sonny");
    let remote = format!("{} link serve {}", quoted(sonny), quoted(&machine_of(link)));
    let mut child = ssh(link)
        .arg(remote)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("ssh could not start: {e}"))?;
    let (Some(mut stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err("ssh has no pipes".into());
    };
    let complaint = tokio::spawn(async move {
        let mut last = String::new();
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(said)) = lines.next_line().await {
            if !said.trim().is_empty() {
                last = said.chars().take(300).collect();
            }
        }
        last
    });
    let (out, mut outgoing) = mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(async move {
        while let Some(value) = outgoing.recv().await {
            if send(&mut stdin, &value).await.is_err() {
                return;
            }
        }
    });
    let verdicts: Verdicts = Arc::default();
    // The calls in flight, ended with the dial however it ends: dropped
    // with it, they are aborted too.
    let mut running = tokio::task::JoinSet::new();
    let mut calls: HashMap<u64, tokio::task::AbortHandle> = HashMap::new();
    let mut stdout = lines(BufReader::new(stdout), LINE_BYTES, None);
    let mut ping = tokio::time::interval(PING);
    let mut refused = None;
    // What the host was last told this machine has, told again as it
    // changes: a server added or taken away, agent-browser installed. Read
    // apart from the calls, since a server starting holds what it knows.
    let mut told: Option<Value> = None;
    let (fresh, mut freshened) = mpsc::channel::<Value>(1);
    let refresher = {
        let (link, hub) = (link.clone(), hub.clone());
        tokio::spawn(async move {
            let mut every = tokio::time::interval(PING);
            loop {
                every.tick().await;
                if fresh.send(lends(&link, &hub).await).await.is_err() {
                    return;
                }
            }
        })
    };
    // Aborted however the dial ends, dropped included.
    let _refresher = AbortOnDrop(refresher);
    loop {
        tokio::select! {
            said = stdout.recv() => {
                let Some(Ok(said)) = said else { break };
                if let Some(hello) = said.get("hello") {
                    if hello["protocol"].as_u64() != Some(PROTOCOL) {
                        refused = Some(format!(
                            "{} runs another Sonny (link protocol {}, here {PROTOCOL}): run the same Sonny on both",
                            link.host, hello["protocol"]
                        ));
                    }
                    let lends = lends(link, hub).await;
                    let _ = out.send(json!({"hello": {"protocol": PROTOCOL, "lends": lends}}));
                    told = Some(lends);
                    stood(key, link, refused.is_none(), refused.clone());
                    continue;
                }
                let Some(id) = said["id"].as_u64() else { continue };
                if let Some(called) = said.get("call") {
                    while running.try_join_next().is_some() {}
                    calls.retain(|_, job| !job.is_finished());
                    let asker = Asker {
                        id,
                        out: out.clone(),
                        verdicts: verdicts.clone(),
                        seq: AtomicU64::new(0),
                    };
                    let (called, link, hub, out, key) =
                        (called.clone(), link.clone(), hub.clone(), out.clone(), key.to_owned());
                    let job = running.spawn(async move {
                        let _ = out.send(json!({"id": id, "ack": true}));
                        let answer = lend(&key, &link, &hub, &called, asker).await;
                        let _ = out.send(json!({"id": id, "done": done(answer)}));
                    });
                    calls.insert(id, job);
                } else if said.get("cancel").is_some() {
                    if let Some(job) = calls.remove(&id) {
                        job.abort();
                    }
                } else if let Some(verdict) = said.get("verdict")
                    && let Some(seq) = said["seq"].as_u64()
                    && let Some(waiting) = verdicts.lock().unwrap().remove(&(id, seq))
                {
                    let _ = waiting.send(verdict.clone());
                }
            }
            _ = ping.tick() => {
                let _ = out.send(json!({"ping": true}));
            }
            Some(lends) = freshened.recv() => {
                if told.is_some() && refused.is_none() && told.as_ref() != Some(&lends) {
                    let _ = out.send(json!({"hello": {"protocol": PROTOCOL, "lends": lends}}));
                    told = Some(lends);
                }
            }
        }
    }
    running.abort_all();
    writer.abort();
    let _ = child.kill().await;
    let said = complaint.await.unwrap_or_default();
    Err(refused.unwrap_or(match said.is_empty() {
        true => "the link closed".into(),
        false => said,
    }))
}

/// A task aborted when this is dropped.
struct AbortOnDrop(tokio::task::JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// What this machine lends over `link`: its browser, while agent-browser
/// is here, its desktop, while agents here can use it, its MCP servers as
/// last listed, and opens.
async fn lends(link: &agent_client::config::Link, hub: &crate::mcp::Hub) -> Value {
    let config = agent_client::config::load().unwrap_or_default();
    hub.configure(&config.sonny.mcp_servers).await;
    let mut servers = serde_json::Map::new();
    for (name, server) in config.sonny.mcp_servers.iter().filter(|(_, s)| s.enabled) {
        let known = hub.known(name).await.unwrap_or_default();
        let tools: Vec<Value> = (known.tools.into_iter())
            .filter(|tool| crate::mcp::allowed(server, tool["name"].as_str().unwrap_or_default()))
            .collect();
        let description = server.description.clone().or(known.instructions);
        servers.insert(
            name.clone(),
            json!({"description": description, "tools": tools}),
        );
    }
    json!({
        "browser": crate::setup::on_path("agent-browser"),
        "signed_in": !link.browser_profile.is_empty(),
        "desktop": crate::desktop::here(),
        "open": true,
        "servers": servers,
    })
}

/// Answer `called`, a call of the agents over `link`, with `asker` putting
/// to their gate what it asks to confirm.
async fn lend(
    key: &str,
    link: &agent_client::config::Link,
    hub: &crate::mcp::Hub,
    called: &Value,
    asker: Asker,
) -> Result<Value, Failed> {
    let payload = &called["payload"];
    let steps = Steps::over(asker);
    let left = Duration::from_millis(payload["timeout_ms"].as_u64().unwrap_or(600_000));
    match called["tool"].as_str() {
        Some(crate::browser::NAME) => {
            let browsed = crate::browser::lent(payload, &link.host, &link.browser_profile, &steps);
            match tokio::time::timeout(left.min(crate::browser::TIMEOUT), browsed).await {
                Ok(answer) => answer,
                Err(_) => Err(("timed_out", "the browser's commands ran out of time".into())),
            }
        }
        Some(crate::desktop::NAME) => {
            let driven = crate::desktop::lent(payload, &steps);
            match tokio::time::timeout(left.min(crate::desktop::TIMEOUT), driven).await {
                Ok(answer) => answer,
                Err(_) => Err(("timed_out", "the desktop's commands ran out of time".into())),
            }
        }
        Some(crate::exec::NAME) => {
            let input = payload["input"].as_str().unwrap_or_default();
            let bot = format!("{key}.{}", payload["bot"].as_str().unwrap_or_default());
            let named = payload["whole"].as_str().map(Path::new);
            // Kept apart from this machine's agents, and by the store there
            // too, since two stores may each have an agent of one name.
            // By a digest of the store's path: a name for a folder, one to
            // a store.
            use sha2::Digest;
            let store = payload["store"].as_str().unwrap_or_default();
            let digest = sha2::Sha256::digest(store.as_bytes());
            let store: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
            let space = Path::new(key).join(store);
            crate::exec::lent(hub, &bot, input, left, steps, named, &space)
                .await
                .map_err(|e| ("exec_refused", e))
        }
        Some("open") => opened(payload)
            .map(Value::String)
            .map_err(|e| ("open_refused", e)),
        other => Err((
            "link_refused",
            format!("{} is not lent here", other.unwrap_or("that")),
        )),
    }
}

/// Open what another machine sent: a web address in the browser, or a file
/// a viewer shows, kept in Sonny's folder and marked as downloaded.
fn opened(payload: &Value) -> Result<String, String> {
    if let Some(url) = payload["url"].as_str() {
        if !url.starts_with("https://") && !url.starts_with("http://") {
            return Err("only web addresses open here".into());
        }
        open::that_detached(url).map_err(|e| e.to_string())?;
        return Ok(format!("opened {url}"));
    }
    let name = payload["file"]["name"].as_str().unwrap_or_default();
    let name: String = (name.chars())
        .map(|c| match c.is_alphanumeric() || "._- ".contains(c) {
            true => c,
            false => '_',
        })
        .collect();
    let name = name.trim_start_matches('.').to_owned();
    if name.is_empty() || !viewed(&name) {
        return Err(format!("{name} is not a file a viewer opens"));
    }
    use base64::Engine;
    let data = payload["file"]["data"].as_str().unwrap_or_default();
    let bytes =
        (base64::engine::general_purpose::STANDARD.decode(data)).map_err(|e| e.to_string())?;
    if bytes.len() > OPEN_BYTES {
        return Err(format!("{name} is more than {} MB", OPEN_BYTES >> 20));
    }
    let dir = mine().ok_or("HOME is not set")?.join("opened");
    private(&dir).map_err(|e| e.to_string())?;
    let file = dir.join(format!("{}-{name}", &nonce()[..8]));
    std::fs::write(&file, bytes).map_err(|e| e.to_string())?;
    if cfg!(target_os = "macos") {
        // As a browser marks what it downloads, so Gatekeeper asks first.
        let mark = format!("0081;{:x};Sonny;", now_ms() / 1000);
        let _ = std::process::Command::new("/usr/bin/xattr")
            .args(["-w", "com.apple.quarantine", &mark])
            .arg(&file)
            .status();
    }
    open::that_detached(&file).map_err(|e| e.to_string())?;
    Ok(format!("opened {name}"))
}

// ---- `sonny link` ----

const USAGE: &str = "usage:
  sonny link HOST [--machine NAME] [--name NAME]   link this machine to HOST, for its agents to use
  sonny link off NAME                              take the link away
  sonny link                                       the links, and the machines linked here";

/// `sonny link ...`.
pub fn cli(args: &[String]) -> Result<(), String> {
    let runtime = || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())
    };
    match args {
        [] => {
            list();
            Ok(())
        }
        [serve, machine] if serve == "serve" => runtime()?.block_on(serve_machine(machine.clone())),
        [forget, machine] if forget == "forget" => forget_machine(machine),
        [off, name] if off == "off" => off_link(name),
        [first, rest @ ..] if !first.starts_with('-') && first != "off" => setup(first, rest),
        _ => Err(USAGE.into()),
    }
}

async fn serve_machine(machine: String) -> Result<(), String> {
    serve(machine).await
}

/// The name a host's link goes by: its first word, as `box` of
/// `me@box.example.com`.
fn name_of(host: &str) -> String {
    let host = host.rsplit_once('@').map_or(host, |(_, host)| host);
    let first = host.split(['.', ':']).next().unwrap_or(host);
    let name: String = (first.chars())
        .map(
            |c| match c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                true => c,
                false => '-',
            },
        )
        .take(64)
        .collect();
    match name.is_empty() {
        true => "link".into(),
        false => name,
    }
}

/// `sonny link HOST`: find Sonny there, installing this version when it is
/// not, then keep the link in the config, which the tool host dials.
fn setup(host: &str, rest: &[String]) -> Result<(), String> {
    let (mut machine, mut name) = (None, None);
    let mut rest = rest.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--machine" => machine = rest.next().cloned(),
            "--name" => name = rest.next().cloned(),
            _ => return Err(USAGE.into()),
        }
    }
    let name = name.unwrap_or_else(|| name_of(host));
    let machine = machine.unwrap_or_else(this_machine);
    if !agent_client::config::named(&name) || !agent_client::config::named(&machine) {
        return Err("a name is 1 to 64 letters, digits, _ or -".into());
    }
    if host.starts_with('-') || host.contains(char::is_whitespace) {
        return Err(format!("{host} is not a host ssh dials"));
    }
    println!("Looking for Sonny on {host}…");
    let found = |host: &str| -> Result<Option<String>, String> {
        let out = std::process::Command::new("ssh")
            .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=20", "--", host])
            .arg("sh -lc 'command -v sonny || { test -x \"$HOME/.local/bin/sonny\" && echo \"$HOME/.local/bin/sonny\"; }'")
            .output()
            .map_err(|e| format!("ssh could not start: {e}"))?;
        match out.status.code() {
            Some(255) => Err(format!(
                "ssh could not reach {host}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )),
            _ => Ok(String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(str::trim)
                .find(|line| line.starts_with('/'))
                .map(str::to_owned)),
        }
    };
    let command = match found(host)? {
        Some(command) => command,
        None => {
            let version = env!("CARGO_PKG_VERSION");
            println!("Sonny is not on {host}; installing {version} there…");
            let install = format!(
                "curl -fsSL https://raw.githubusercontent.com/lydakis/agent/main/install.sh | SONNY_VERSION={version} sh"
            );
            let status = std::process::Command::new("ssh")
                .args(["-t", "--", host, &install])
                .status()
                .map_err(|e| format!("ssh could not start: {e}"))?;
            if !status.success() {
                return Err(format!("installing Sonny on {host} failed"));
            }
            found(host)?.ok_or(format!("Sonny is still not found on {host}"))?
        }
    };
    let there = std::process::Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "--",
            host,
            &format!("{} --version", quoted(&command)),
        ])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .unwrap_or_default();
    let path = agent_client::config::path().ok_or("HOME is not set")?;
    let mut table = toml::Table::new();
    table.insert("host".into(), toml::Value::String(host.to_owned()));
    table.insert("machine".into(), toml::Value::String(machine.clone()));
    table.insert("command".into(), toml::Value::String(command.clone()));
    agent_client::config::set_table(&path, &format!("sonny.links.{name}"), Some(&table))?;
    println!("{host} runs {there}");
    println!(
        "Linked. While this machine's daemon runs, {host}'s agents can use its browser, its MCP servers and opens, as the machine `{machine}`."
    );
    if !crate::boot::on() {
        agent_client::config::write(
            &path,
            &[("sonny.start_at_boot", Some(toml::Value::Boolean(true)))],
        )?;
        println!(
            "The daemon now {}, so the link comes back by itself.",
            crate::boot::apply(true)?
        );
    }
    Ok(())
}

/// `sonny link off NAME`: the link let go, and the machine it reached told
/// to forget this one.
fn off_link(name: &str) -> Result<(), String> {
    let path = agent_client::config::path().ok_or("HOME is not set")?;
    let config = agent_client::config::read(&path)?;
    let (key, link) = (config.sonny.links.iter())
        .find(|(key, link)| *key == name || link.host == name)
        .ok_or(format!("no link {name}"))?;
    agent_client::config::set_table(&path, &format!("sonny.links.{key}"), None)?;
    let sonny = link.command.as_deref().unwrap_or("sonny");
    let machine = machine_of(link);
    let forget = format!("{} link forget {}", quoted(sonny), quoted(&machine));
    let told = std::process::Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=20",
            "--",
            &link.host,
            &forget,
        ])
        .status()
        .is_ok_and(|status| status.success());
    if let Some(dir) = mine() {
        let _ = std::fs::remove_file(dir.join(format!("{key}.json")));
    }
    match told {
        true => println!(
            "{key} is off; {}'s agents no longer see {machine}",
            link.host
        ),
        false => println!(
            "{key} is off; {} was not reached, so its agents see {machine} until it is: `sonny link forget {machine}` there",
            link.host
        ),
    }
    Ok(())
}

/// `sonny link forget MACHINE`: a machine linked here taken out of its
/// agents' tools.
fn forget_machine(machine: &str) -> Result<(), String> {
    if !agent_client::config::named(machine) {
        return Err(format!("no machine {machine}"));
    }
    let dir = here().ok_or("HOME is not set")?;
    for suffix in ["json", "seen"] {
        let _ = std::fs::remove_file(dir.join(format!("{machine}.{suffix}")));
    }
    Ok(())
}

/// `sonny link`: this machine's links, and the machines linked here.
fn list() {
    let (config, loaded) = match agent_client::config::load() {
        Ok(config) => (config, true),
        Err(error) => {
            println!("This machine's links are unknown while its config does not load: {error}");
            (agent_client::config::Config::default(), false)
        }
    };
    for (key, link) in &config.sonny.links {
        let said: Value = (mine()
            .and_then(|dir| std::fs::read(dir.join(format!("{key}.json"))).ok()))
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
        let how = match (said["connected"].as_bool(), said["error"].as_str()) {
            (Some(true), _) => "connected".to_owned(),
            (_, Some(error)) => format!("not connected: {error}"),
            _ => "not connected yet".to_owned(),
        };
        println!(
            "{key}  this machine, linked to {} as {} · {how}",
            link.host,
            machine_of(link)
        );
    }
    for machine in machines() {
        let how = match seen(&machine.name) {
            Some(ms) if now_ms().saturating_sub(ms) < SILENT.as_millis() as u64 => {
                "connected".to_owned()
            }
            Some(ms) => format!("last connected {}", ago(ms)),
            None => "not connected yet".to_owned(),
        };
        let mut lends = Vec::new();
        if machine.browser {
            lends.push("browser".to_owned());
        }
        if machine.desktop {
            lends.push("desktop".to_owned());
        }
        if !machine.servers.is_empty() {
            lends.push(format!("{} MCP servers", machine.servers.len()));
        }
        if machine.open {
            lends.push("opens".to_owned());
        }
        println!(
            "{}  linked here · {how} · {}",
            machine.name,
            lends.join(", ")
        );
    }
    if loaded && config.sonny.links.is_empty() && machines().is_empty() {
        println!("No links. {USAGE}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_s_link_is_named_by_its_first_word() {
        assert!(agent_client::config::named(&this_machine()));
        assert_eq!(name_of("box"), "box");
        assert_eq!(name_of("me@devbox.example.com"), "devbox");
        assert_eq!(name_of("me@10.0.0.2"), "10");
        assert_eq!(name_of("we_ird!"), "we_ird-");
    }

    #[test]
    fn only_what_a_viewer_shows_is_opened() {
        assert!(viewed("chart.PNG"));
        assert!(viewed("plan.pdf"));
        assert!(!viewed("run.command"));
        assert!(!viewed("app.html"));
        assert!(!viewed("noext"));
        assert!(opened(&json!({"url": "file:///etc/passwd"})).is_err());
        assert!(opened(&json!({"url": "javascript:alert(1)"})).is_err());
        assert!(opened(&json!({"file": {"name": "x.sh", "data": ""}})).is_err());
    }

    #[test]
    fn a_remote_word_is_quoted_whatever_it_holds() {
        assert_eq!(quoted("sonny"), "'sonny'");
        assert_eq!(quoted("a'b; rm"), "'a'\\''b; rm'");
    }

    #[test]
    fn an_error_comes_back_with_its_code_or_one_of_ours() {
        assert_eq!(
            ended(&done(Err(("timed_out", "slow".into())))),
            Err(("timed_out", "slow".into()))
        );
        let made_up = json!({"error": {"code": "anything", "detail": "x"}});
        assert_eq!(ended(&made_up), Err(("link_refused", "x".into())));
        assert_eq!(ended(&done(Ok(json!("hi")))), Ok(json!("hi")));
    }

    #[tokio::test]
    async fn a_line_that_comes_in_pieces_is_read_whole_whatever_else_wakes() {
        let (mut write, read) = tokio::io::duplex(64);
        let mut said = lines(BufReader::new(read), 1024, None);
        tokio::spawn(async move {
            write.write_all(b"{\"a\":").await.unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            write.write_all(b"1}\n").await.unwrap();
        });
        let mut tick = tokio::time::interval(Duration::from_millis(5));
        let got = loop {
            tokio::select! {
                got = said.recv() => break got,
                _ = tick.tick() => {}
            }
        };
        assert_eq!(got.unwrap().unwrap(), json!({"a": 1}));
        assert!(said.recv().await.is_none());
    }

    #[tokio::test]
    async fn a_line_too_long_is_refused_and_one_at_its_end_is_none() {
        let mut short: &[u8] = b"{\"a\":1}\n";
        assert_eq!(
            line(&mut short, 100, None).await.unwrap(),
            Some(json!({"a": 1}))
        );
        assert_eq!(line(&mut short, 100, None).await.unwrap(), None);
        let mut long: &[u8] = b"{\"a\":\"0123456789\"}\n";
        assert!(line(&mut long, 8, None).await.is_err());
    }
}
