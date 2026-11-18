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
/// agent's reply to it.
fn kept() -> Value {
    json!([
        {"node": 0, "message": 0, "item": {"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "An earlier question"}]}},
        {"node": 1, "message": 1, "item": {"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "Count the tallies"}]}},
        {"node": 2, "item": {"type": "message", "role": "assistant",
            "content": [{"type": "output_text",
                "text": "First tally done\nSecond tally done\nAll tallies counted"}]}},
    ])
}

/// A daemon that holds back every answer by `held`, each on its own, and
/// sends the events `told` gives it. What it was asked is in the list.
async fn held_daemon(
    held: Duration,
) -> (
    Arc<Client>,
    Events,
    tokio::sync::mpsc::UnboundedSender<Value>,
    Arc<Mutex<Vec<(Instant, Value)>>>,
) {
    static STARTED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "sonny-held-{}-{}",
        std::process::id(),
        STARTED.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("agent.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let (tell, mut told) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let asked = Arc::new(Mutex::new(Vec::new()));
    let log = asked.clone();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read, write) = stream.into_split();
        let write = Arc::new(tokio::sync::Mutex::new(write));
        let ready = json!({"event": "ready", "protocol": agent_client::PROTOCOL,
            "providers": {}});
        (write.lock().await)
            .write_all(format!("{ready}\n").as_bytes())
            .await
            .unwrap();
        let events = write.clone();
        tokio::spawn(async move {
            while let Some(event) = told.recv().await {
                let line = format!("{event}\n");
                let _ = events.lock().await.write_all(line.as_bytes()).await;
            }
        });
        let mut lines = BufReader::new(read).lines();
        let mut message = 1;
        while let Ok(Some(line)) = lines.next_line().await {
            let asked: Value = serde_json::from_str(&line).unwrap();
            log.lock().unwrap().push((Instant::now(), asked.clone()));
            let entries = kept();
            let result = match asked["op"].as_str() {
                Some("history_items") => json!({"items": entries.as_array().into_iter()
                    .flatten()
                    .filter(|entry| asked["nodes"].as_array().into_iter().flatten()
                        .any(|node| *node == entry["node"]))
                    .cloned()
                    .collect::<Vec<_>>()}),
                // What is older than the window drew: one node.
                Some("history_nodes") => {
                    json!({"nodes": [{"node": 0, "turn": 0}], "next_from": null})
                }
                Some("submit") => {
                    message += 1;
                    json!({"message": message})
                }
                Some("inspect") => json!({"name": "b"}),
                _ => json!({}),
            };
            let answer = json!({"id": asked["id"], "result": result});
            let write = write.clone();
            tokio::spawn(async move {
                tokio::time::sleep(held).await;
                let _ = write
                    .lock()
                    .await
                    .write_all(format!("{answer}\n").as_bytes())
                    .await;
            });
        }
    });
    let (client, events) = Client::connect(&socket).await.unwrap();
    (client, events, tell, asked)
}

fn window(client: Arc<Client>) -> App {
    let options = Options {
        store: PathBuf::new(),
        socket: PathBuf::new(),
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
    let (client, mut events, tell, asked) = held_daemon(HELD).await;
    let mut app = window(client);
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
    assert!(
        p99 < Duration::from_millis(50),
        "key to character p99 {p99:?}"
    );
    assert!(worst < Duration::from_millis(100), "a key waited {worst:?}");
    // What was sent showed at once, staged, and went to the daemon.
    let staged = frames.iter().find(|(at, row, whole)| {
        *at >= entered && row.ends_with("› ") && whole.contains(typed.as_str())
    });
    let staged_in = staged.map(|(at, ..)| *at - entered);
    assert!(
        staged_in.is_some_and(|took| took < Duration::from_millis(100)),
        "staged after {staged_in:?}"
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
