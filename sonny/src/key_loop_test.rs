//! The window's loop never waits on the daemon: with every request the
//! daemon answers held back, typing, deleting and scrolling still show at
//! once, and the transcript catches up in order once the reads come back.

use super::*;
use std::sync::Mutex;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// How long the stand-in daemon holds back every answer.
const HELD: Duration = Duration::from_secs(5);

/// A screen that keeps every frame drawn on it: when it was drawn, and
/// what the row under the cursor said left of the cursor, which is what
/// the composer shows typed.
struct Probe {
    screen: ratatui::backend::TestBackend,
    frames: Arc<Mutex<Vec<(Instant, String, String)>>>,
}

impl Probe {
    fn typed_now(&mut self) -> (String, String) {
        let at = self.screen.get_cursor_position().unwrap_or_default();
        let buffer = self.screen.buffer();
        let width = buffer.area.width;
        let row = |y: u16| -> String {
            (0..width)
                .map(|x| buffer[(x, y)].symbol().to_owned())
                .collect::<String>()
        };
        let typed: String = row(at.y).chars().take(at.x as usize).collect();
        let whole = (0..buffer.area.height)
            .map(row)
            .collect::<Vec<_>>()
            .join("\n");
        (typed, whole)
    }
}

impl std::io::Write for Probe {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Backend for Probe {
    type Error = std::io::Error;
    fn draw<'a, I>(&mut self, content: I) -> std::io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
    {
        let Ok(()) = self.screen.draw(content);
        Ok(())
    }
    fn hide_cursor(&mut self) -> std::io::Result<()> {
        let Ok(()) = self.screen.hide_cursor();
        Ok(())
    }
    fn show_cursor(&mut self) -> std::io::Result<()> {
        let Ok(()) = self.screen.show_cursor();
        Ok(())
    }
    fn get_cursor_position(&mut self) -> std::io::Result<ratatui::layout::Position> {
        let Ok(at) = self.screen.get_cursor_position();
        Ok(at)
    }
    fn set_cursor_position<P: Into<ratatui::layout::Position>>(
        &mut self,
        position: P,
    ) -> std::io::Result<()> {
        let Ok(()) = self.screen.set_cursor_position(position);
        Ok(())
    }
    fn clear(&mut self) -> std::io::Result<()> {
        let Ok(()) = self.screen.clear();
        Ok(())
    }
    fn clear_region(&mut self, clear_type: ratatui::backend::ClearType) -> std::io::Result<()> {
        let Ok(()) = self.screen.clear_region(clear_type);
        Ok(())
    }
    fn size(&self) -> std::io::Result<ratatui::layout::Size> {
        let Ok(size) = self.screen.size();
        Ok(size)
    }
    fn window_size(&mut self) -> std::io::Result<ratatui::backend::WindowSize> {
        let Ok(size) = self.screen.window_size();
        Ok(size)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        let (typed, whole) = self.typed_now();
        (self.frames.lock().unwrap()).push((Instant::now(), typed, whole));
        Ok(())
    }
    fn scroll_region_up(
        &mut self,
        region: std::ops::Range<u16>,
        lines: u16,
    ) -> std::io::Result<()> {
        let Ok(()) = self.screen.scroll_region_up(region, lines);
        Ok(())
    }
    fn scroll_region_down(
        &mut self,
        region: std::ops::Range<u16>,
        lines: u16,
    ) -> std::io::Result<()> {
        let Ok(()) = self.screen.scroll_region_down(region, lines);
        Ok(())
    }
}

/// The items bot `b` has, by node: a turn the person started and the
/// agent's reply to it, `reply`.
fn kept(reply: &str) -> Value {
    json!([
        {"node": 0, "message": 0, "item": {"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "An earlier question"}]}},
        {"node": 1, "message": 1, "item": {"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "Count the tallies"}]}},
        {"node": 2, "item": {"type": "message", "role": "assistant",
            "content": [{"type": "output_text",
                "text": reply}]}},
    ])
}

/// A daemon that holds back every answer by `held`, each on its own, and
/// sends the events `told` gives it on the first connection; the bot's
/// items are `items`, its nodes as `history_nodes` lists them `nodes`.
/// A window that connects again is answered the same way, its follow
/// live at once. What it was asked is in the list.
async fn held_daemon(
    held: Duration,
    items: Value,
    nodes: Value,
) -> (
    Arc<Client>,
    Events,
    tokio::sync::mpsc::UnboundedSender<Value>,
    Arc<Mutex<Vec<(Instant, Value)>>>,
    PathBuf,
) {
    static STARTED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "sonny-held-{}-{}",
        std::process::id(),
        STARTED.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("agent.sock");
    // One left by an earlier run under the same process id.
    let _ = std::fs::remove_file(&socket);
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let (tell, told) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let asked = Arc::new(Mutex::new(Vec::new()));
    let log = asked.clone();
    tokio::spawn(async move {
        let mut told = Some(told);
        let message = Arc::new(std::sync::atomic::AtomicI64::new(1));
        while let Ok((stream, _)) = listener.accept().await {
            let (read, write) = stream.into_split();
            let write = Arc::new(tokio::sync::Mutex::new(write));
            let ready = json!({"event": "ready", "protocol": agent_client::PROTOCOL,
                "providers": {}});
            (write.lock().await)
                .write_all(format!("{ready}\n").as_bytes())
                .await
                .unwrap();
            if let Some(mut told) = told.take() {
                let events = write.clone();
                tokio::spawn(async move {
                    while let Some(event) = told.recv().await {
                        let line = format!("{event}\n");
                        let _ = events.lock().await.write_all(line.as_bytes()).await;
                    }
                });
            }
            let (log, items, nodes, message) =
                (log.clone(), items.clone(), nodes.clone(), message.clone());
            tokio::spawn(async move {
                let mut lines = BufReader::new(read).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let asked: Value = serde_json::from_str(&line).unwrap();
                    log.lock().unwrap().push((Instant::now(), asked.clone()));
                    let result = match asked["op"].as_str() {
                        Some("history_items") => json!({"items": items.as_array().into_iter()
                            .flatten()
                            .filter(|entry| asked["nodes"].as_array().into_iter().flatten()
                                .any(|node| *node == entry["node"]))
                            .cloned()
                            .collect::<Vec<_>>()}),
                        Some("history_nodes") => json!({"nodes": nodes, "next_from": null}),
                        Some("submit") => json!({"message":
                            message.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1}),
                        Some("inspect") => json!({"name": "b"}),
                        _ => json!({}),
                    };
                    let answer = json!({"id": asked["id"], "result": result});
                    let follow = asked["op"] == "follow";
                    let write = write.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(held).await;
                        let mut write = write.lock().await;
                        let _ = write.write_all(format!("{answer}\n").as_bytes()).await;
                        if follow {
                            let live = json!({"event": "follow_live"});
                            let _ = write.write_all(format!("{live}\n").as_bytes()).await;
                        }
                    });
                }
            });
        }
    });
    let (client, events) = Client::connect(&socket).await.unwrap();
    (client, events, tell, asked, socket)
}

fn window(client: Arc<Client>, socket: PathBuf) -> App {
    let options = Options {
        store: PathBuf::new(),
        socket,
        bot: None,
        new: false,
        model: None,
        workspace: None,
        layout: false,
        rex_host: None,
        sound: false,
        side: None,
        embedded: false,
        window_place: None,
        lead: project::Pick::Last,
        send: None,
        draft: None,
        reopened: false,
    };
    App::new(client, "b", &json!({"name": "b"}), options)
}

fn key(code: KeyCode) -> Event {
    Event::Key(event::KeyEvent::new(code, KeyModifiers::NONE))
}

/// The time from each key to the first frame that shows what it typed.
fn latencies(sent: &[(Instant, String)], frames: &[(Instant, String, String)]) -> Vec<Duration> {
    sent.iter()
        .map(|(at, typed)| {
            let shown = frames.iter().find(|(drawn, row, _)| {
                drawn >= at && row.split_once("› ").map(|(_, typed)| typed) == Some(typed)
            });
            match shown {
                Some((drawn, ..)) => *drawn - *at,
                None => Duration::MAX,
            }
        })
        .collect()
}

fn percentile(sorted: &[Duration], at: f64) -> Duration {
    sorted[((sorted.len() - 1) as f64 * at).round() as usize]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typing_and_scrolling_never_wait_on_a_daemon_that_holds_every_answer() {
    let nodes = json!([{"node": 0, "turn": 0}]);
    let (client, mut events, tell, asked, socket) = held_daemon(
        HELD,
        kept("First tally done\nSecond tally done\nAll tallies counted"),
        nodes,
    )
    .await;
    let mut app = window(client, socket);
    // Older items to read as the view nears the first row drawn.
    app.older = Some(1);
    let frames = Arc::new(Mutex::new(Vec::new()));
    let probe = Probe {
        screen: ratatui::backend::TestBackend::new(100, 30),
        frames: frames.clone(),
    };
    let mut terminal = Terminal::new(probe).unwrap();
    let (keys, taken) = tokio::sync::mpsc::unbounded_channel();
    let began = Instant::now();
    let drive = async {
        // The agent's turn starts, streams its reply and ends: each event
        // that needs a read waits 5 s for it.
        let streamed = [
            "First tally done\n",
            "Second tally done\n",
            "All tallies counted",
        ];
        tell.send(json!({"event": "accepted", "bot": "b", "turn": 1,
            "data": {"node": 1}}))
            .unwrap();
        for piece in streamed {
            tell.send(
                json!({"event": "text_delta", "bot": "b", "turn": 1, "item": 0,
                "text": piece}),
            )
            .unwrap();
        }
        tell.send(json!({"event": "message", "bot": "b", "turn": 1,
            "data": {"node": 2}}))
            .unwrap();
        tell.send(json!({"event": "turn_finished", "bot": "b", "turn": 1,
            "data": {"status": "completed"}}))
            .unwrap();
        // Meanwhile the person types, deletes, scrolls and types on.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut sent: Vec<(Instant, String)> = Vec::new();
        let mut typed = String::new();
        let notch = |kind| {
            Event::Mouse(event::MouseEvent {
                kind,
                column: 1,
                row: 1,
                modifiers: KeyModifiers::NONE,
            })
        };
        for (at, c) in "the tallies look right".chars().enumerate() {
            typed.push(c);
            keys.send(key(KeyCode::Char(c))).unwrap();
            sent.push((Instant::now(), typed.clone()));
            tokio::time::sleep(Duration::from_millis(40)).await;
            if at % 5 == 4 {
                typed.pop();
                keys.send(key(KeyCode::Backspace)).unwrap();
                sent.push((Instant::now(), typed.clone()));
                tokio::time::sleep(Duration::from_millis(40)).await;
                keys.send(notch(event::MouseEventKind::ScrollUp)).unwrap();
                keys.send(notch(event::MouseEventKind::ScrollDown)).unwrap();
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
        }
        // Sent: the composer empties at once, and the message shows
        // where it will land while the daemon holds its answer.
        keys.send(key(KeyCode::Enter)).unwrap();
        let entered = Instant::now();
        sent.push((entered, String::new()));
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut more = String::new();
        for c in "and more".chars() {
            more.push(c);
            keys.send(key(KeyCode::Char(c))).unwrap();
            sent.push((Instant::now(), more.clone()));
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        // The reads come back: the transcript catches up.
        tokio::time::sleep(HELD * 3).await;
        for _ in 0.."and more".len() {
            keys.send(key(KeyCode::Backspace)).unwrap();
        }
        for c in "/quit".chars() {
            keys.send(key(KeyCode::Char(c))).unwrap();
        }
        keys.send(key(KeyCode::Enter)).unwrap();
        (sent, typed, entered)
    };
    let (ran, (sent, typed, entered)) = tokio::time::timeout(Duration::from_secs(60), async {
        tokio::join!(app.run(&mut terminal, &mut events, taken), drive)
    })
    .await
    .expect("the window quit");
    ran.unwrap();
    let frames = frames.lock().unwrap().clone();
    let mut waited = latencies(&sent, &frames);
    waited.sort();
    let p99 = percentile(&waited, 0.99);
    let worst = *waited.last().unwrap();
    eprintln!(
        "keys {} p50 {:?} p99 {:?} worst {:?}; reads asked {}; {:?} in all",
        waited.len(),
        percentile(&waited, 0.5),
        p99,
        worst,
        asked.lock().unwrap().len(),
        began.elapsed(),
    );
    at_once(worst, HELD);
    // What was sent showed at once, staged, and went to the daemon.
    let staged = frames.iter().find(|(at, row, whole)| {
        *at >= entered && row.ends_with("› ") && whole.contains(typed.as_str())
    });
    let staged_in = staged.map(|(at, ..)| *at - entered);
    assert!(
        staged_in.is_some_and(|took| took < HELD / 2),
        "staged after {staged_in:?}, the daemon holding every answer {HELD:?}"
    );
    let submitted = (asked.lock().unwrap().iter())
        .find(|(_, asked)| asked["op"] == "submit")
        .map(|(_, asked)| asked["prompt"].clone());
    assert_eq!(submitted, Some(json!(typed)));
    // The older page and the turn drew, in order: what came before, the
    // person's words, then the reply.
    let whole = &frames.last().unwrap().2;
    let screen = app.screen.texts().join("\n");
    let earlier = screen.find("An earlier question");
    let said = screen.find("Count the tallies");
    let replied = screen.find("All tallies counted");
    assert!(
        earlier.is_some() && said.is_some() && replied.is_some(),
        "{screen}\n---\n{whole}"
    );
    assert!(earlier < said && said < replied, "{screen}");
}

/// How long the stand-in daemon holds the read a stream waits behind.
const STREAM_HELD: Duration = Duration::from_secs(2);

/// A reply of `pieces` lines streamed fast behind an event whose read the
/// daemon holds 2 s, while the person types: the window after it quit,
/// what it drew, the keys, and the lines.
async fn stream_behind_a_held_read(
    pieces: usize,
) -> (
    App,
    Vec<(Instant, String, String)>,
    Vec<(Instant, String)>,
    Vec<String>,
    Arc<Mutex<Vec<(Instant, Value)>>>,
) {
    let lines: Vec<String> = (0..pieces).map(|n| format!("tally {n:05}\n")).collect();
    let reply = lines.concat();
    let items = json!([
        {"node": 1, "message": 1, "item": {"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "Count the tallies"}]}},
        {"node": 2, "item": {"type": "message", "role": "assistant",
            "content": [{"type": "output_text", "text": reply.trim_end()}]}},
    ]);
    let nodes = json!([{"node": 2}, {"node": 1}]);
    let held = STREAM_HELD;
    let (client, mut events, tell, asked, socket) = held_daemon(held, items, nodes).await;
    let mut app = window(client, socket);
    let frames = Arc::new(Mutex::new(Vec::new()));
    let probe = Probe {
        screen: ratatui::backend::TestBackend::new(100, 30),
        frames: frames.clone(),
    };
    let mut terminal = Terminal::new(probe).unwrap();
    let (keys, taken) = tokio::sync::mpsc::unbounded_channel();
    let drive = async {
        // The turn's start needs a read the daemon holds; every piece of
        // the reply comes behind it at once.
        tell.send(json!({"event": "accepted", "bot": "b", "turn": 1,
            "data": {"node": 1}}))
            .unwrap();
        for piece in &lines {
            tell.send(json!({"event": "text_delta", "bot": "b", "turn": 1,
                "item": 0, "text": piece}))
                .unwrap();
        }
        tell.send(json!({"event": "message", "bot": "b", "turn": 1,
            "data": {"node": 2}}))
            .unwrap();
        tell.send(json!({"event": "turn_finished", "bot": "b", "turn": 1,
            "data": {"status": "completed"}}))
            .unwrap();
        // The person types while the read is held, while the pieces are
        // taken after it, and while the window catches up, until the
        // reply's last line shows.
        let last = lines.last().unwrap().trim_end().to_owned();
        let shown = || {
            let frames = frames.lock().unwrap();
            frames
                .last()
                .is_some_and(|(_, _, whole)| whole.contains(&last))
        };
        let mut sent: Vec<(Instant, String)> = Vec::new();
        let mut typed = String::new();
        let typing = Instant::now();
        while (typing.elapsed() < held * 3 || !shown())
            && typing.elapsed() < Duration::from_secs(45)
        {
            for c in "tallies".chars() {
                typed.push(c);
                keys.send(key(KeyCode::Char(c))).unwrap();
                sent.push((Instant::now(), typed.clone()));
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
            // Cleared, so the words stay on one row.
            typed.clear();
            keys.send(Event::Key(event::KeyEvent::new(
                KeyCode::Char('u'),
                KeyModifiers::CONTROL,
            )))
            .unwrap();
            sent.push((Instant::now(), typed.clone()));
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        // Everything heard, or caught up from history, the window quits.
        tokio::time::sleep(held).await;
        for c in "/quit".chars() {
            keys.send(key(KeyCode::Char(c))).unwrap();
        }
        keys.send(key(KeyCode::Enter)).unwrap();
        sent
    };
    let (ran, sent) = tokio::time::timeout(Duration::from_secs(60), async {
        tokio::join!(app.run(&mut terminal, &mut events, taken), drive)
    })
    .await
    .expect("the window quit");
    ran.unwrap();
    let frames = frames.lock().unwrap().clone();
    (app, frames, sent, lines, asked)
}

/// A key showed before the answer held back could have come. A loop that
/// waited on that answer shows a key typed as it was asked only once the
/// answer comes, so a key showing in under half the hold did not wait on
/// it. That holds on a loaded machine, whose scheduler can stop any
/// thread a few hundred milliseconds, where a figure in milliseconds
/// fails: the latencies are printed for the record instead.
fn at_once(worst: Duration, held: Duration) {
    assert!(
        worst < held / 2,
        "a key waited {worst:?}, the answer held back {held:?}"
    );
}

/// Keys showed at once, and the reply is there whole, every line once and
/// in order, after the person's words.
fn keys_and_reply_whole(
    app: &App,
    frames: &[(Instant, String, String)],
    sent: &[(Instant, String)],
    lines: &[String],
) {
    keys_at_once(frames, sent, STREAM_HELD);
    reply_whole(app, lines);
}

/// Every key showed at once.
fn keys_at_once(frames: &[(Instant, String, String)], sent: &[(Instant, String)], held: Duration) {
    let mut waited = latencies(sent, frames);
    waited.sort();
    let p99 = percentile(&waited, 0.99);
    let worst = *waited.last().unwrap();
    eprintln!(
        "keys {} p50 {:?} p99 {:?} worst {:?}",
        waited.len(),
        percentile(&waited, 0.5),
        p99,
        worst,
    );
    at_once(worst, held);
}

/// The reply is there whole, every line once and in order, after the
/// person's words.
fn reply_whole(app: &App, lines: &[String]) {
    let screen = app.screen.texts().join("\n");
    let mut from = screen
        .find("Count the tallies")
        .expect("the person's words");
    for line in lines {
        let line = line.trim_end();
        let at = screen[from..]
            .find(line)
            .unwrap_or_else(|| panic!("{line} missing or out of order"));
        from += at + line.len();
        assert!(!screen[from..].contains(line), "{line} drawn twice");
    }
}

/// How many times the window connected again, following its agent anew
/// each time (the test's own connection is followed from the start).
fn reconnects(asked: &Mutex<Vec<(Instant, Value)>>) -> usize {
    let asked = asked.lock().unwrap();
    asked
        .iter()
        .filter(|(_, asked)| asked["op"] == "follow")
        .count()
}

/// A reply streamed fast behind an event whose read the daemon holds: the
/// window takes one event at a time (its type holds no more), the rest
/// waiting in the connection's bounded queue, which holds them all here
/// with no reconnect; keys show at once throughout; and the reply ends
/// whole and in order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fast_stream_behind_a_held_read_stays_bounded_and_lands_in_order() {
    let (app, frames, sent, lines, asked) = stream_behind_a_held_read(3_000).await;
    assert_eq!(
        reconnects(&asked),
        0,
        "the connection's queue held the stream"
    );
    keys_and_reply_whole(&app, &frames, &sent, &lines);
}

/// More than the connection's queue holds behind a held read: the
/// connection lets go, as it does for any reader that falls behind, and
/// the window connects again and draws what it missed from history,
/// whole and in order; keys show at once throughout, the catching up read
/// beside the loop as every other read is.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stream_past_the_connection_s_queue_is_caught_up_from_history() {
    let (app, frames, sent, lines, asked) = stream_behind_a_held_read(6_000).await;
    assert_eq!(reconnects(&asked), 1, "connected again once");
    keys_and_reply_whole(&app, &frames, &sent, &lines);
}

/// How long the stand-in browser takes to draw a page: as a cold Chrome
/// can.
const DRAWING: Duration = Duration::from_secs(3);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typing_never_waits_on_a_page_the_browser_is_slow_to_draw() {
    use resvg::tiny_skia;
    // A window that draws pictures, as Kitty's does, and a browser that
    // takes 3 s to draw a page: a cold one, its first time.
    term::draws_pictures(true);
    let dir = std::env::temp_dir().join(format!("sonny-slow-page-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut page = tiny_skia::Pixmap::new(64, 32).unwrap();
    page.fill(tiny_skia::Color::BLACK);
    page.save_png(dir.join("drawn.png")).unwrap();
    let browser = dir.join("chrome");
    crate::testing::script(
        &browser,
        &format!(
            "#!/bin/sh\nsleep {}\n\
             for a; do case $a in --screenshot=*) cp '{drawn}' \"${{a#--screenshot=}}\";; esac; done\n",
            DRAWING.as_secs(),
            drawn = dir.join("drawn.png").display(),
        ),
    );
    *crate::html::STAND_IN.lock().unwrap() = Some(browser);
    // A page no earlier run kept drawn.
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let reply = format!("Here is the page\n```html\n<h1>{nonce}</h1>\n```\nThat was the page");
    let nodes = json!([{"node": 0, "turn": 0}]);
    let (client, mut events, tell, _, socket) =
        held_daemon(Duration::ZERO, kept(&reply), nodes).await;
    let mut app = window(client, socket);
    let frames = Arc::new(Mutex::new(Vec::new()));
    let probe = Probe {
        screen: ratatui::backend::TestBackend::new(100, 30),
        frames: frames.clone(),
    };
    let mut terminal = Terminal::new(probe).unwrap();
    let (keys, taken) = tokio::sync::mpsc::unbounded_channel();
    // On a thread of its own: a window that stops to draw stops whatever
    // runs on its own thread too, and the keys must come meanwhile.
    let streamed = reply.clone();
    let drive = tokio::spawn(async move {
        tell.send(json!({"event": "accepted", "bot": "b", "turn": 1,
            "data": {"node": 1}}))
            .unwrap();
        for line in streamed.split_inclusive('\n') {
            tell.send(
                json!({"event": "text_delta", "bot": "b", "turn": 1, "item": 0,
                "text": line}),
            )
            .unwrap();
        }
        tell.send(json!({"event": "message", "bot": "b", "turn": 1,
            "data": {"node": 2}}))
            .unwrap();
        tell.send(json!({"event": "turn_finished", "bot": "b", "turn": 1,
            "data": {"status": "completed"}}))
            .unwrap();
        // The person types on while the page is drawn.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut sent: Vec<(Instant, String)> = Vec::new();
        let mut typed = String::new();
        for c in "the page looks right".chars() {
            typed.push(c);
            keys.send(key(KeyCode::Char(c))).unwrap();
            sent.push((Instant::now(), typed.clone()));
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        // Drawn by now: it takes the row that stood for it.
        tokio::time::sleep(DRAWING * 2).await;
        for _ in 0..typed.len() {
            keys.send(key(KeyCode::Backspace)).unwrap();
        }
        for c in "/quit".chars() {
            keys.send(key(KeyCode::Char(c))).unwrap();
        }
        keys.send(key(KeyCode::Enter)).unwrap();
        sent
    });
    let ran = tokio::time::timeout(
        Duration::from_secs(60),
        app.run(&mut terminal, &mut events, taken),
    )
    .await
    .expect("the window quit");
    let sent = drive.await.unwrap();
    term::draws_pictures(false);
    *crate::html::STAND_IN.lock().unwrap() = None;
    ran.unwrap();
    let frames = frames.lock().unwrap().clone();
    let mut waited = latencies(&sent, &frames);
    waited.sort();
    let p99 = percentile(&waited, 0.99);
    let worst = *waited.last().unwrap();
    eprintln!(
        "keys {} p50 {:?} p99 {:?} worst {:?}",
        waited.len(),
        percentile(&waited, 0.5),
        p99,
        worst,
    );
    at_once(worst, DRAWING);
    // The reply showed at once, a row standing for its page; then the page
    // in that row's place, the words around it as they were. The words come
    // one delta at a time, so a frame may show the row before the words
    // after it have come: some frame shows both.
    let waiting: Vec<_> = frames
        .iter()
        .filter(|(.., whole)| whole.contains("drawing the page…"))
        .collect();
    assert!(
        waiting
            .iter()
            .any(|(.., whole)| whole.contains("That was the page")),
        "{} frames showed the row standing for the page, the first {:?}; the last frame {:?}",
        waiting.len(),
        waiting.first(),
        frames.last()
    );
    let screen = app.screen.texts().join("\n");
    assert!(!screen.contains("drawing the page…"), "{screen}");
    let (before, picture, after) = (
        screen.find("Here is the page"),
        screen.find(crate::image::PLACEHOLDER),
        screen.find("That was the page"),
    );
    assert!(
        before.is_some() && picture.is_some() && after.is_some(),
        "{screen}"
    );
    assert!(before < picture && picture < after, "{screen}");
    let _ = std::fs::remove_dir_all(&dir);
}
