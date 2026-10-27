//! A file the person reads while the lead keeps writing it. `sonny view
//! FILE` shows it in a pane beside the agent (a Rex block or Herdr pane),
//! drawn as Sonny draws a reply's Markdown, pictures and pages, and drawn
//! again whenever the file changes. Run in the person's own terminal, it
//! shows it there. `--browser` opens it in the browser instead, from a
//! small server on this machine, and the tab reloads itself whenever the
//! file changes; the server is Sonny run again, apart from the shell that
//! asked, and goes once no tab has looked for a while.
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    crossterm::{
        event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
        execute,
        terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
    },
    style::Stylize,
    text::{Line, Span},
    widgets::Paragraph,
};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub const USAGE: &str = "usage: sonny view FILE [--browser]";
/// How long the server waits for a tab before it goes.
const IDLE: Duration = Duration::from_secs(10 * 60);
/// How tall a page is laid out in the pane, in CSS pixels, before its
/// blank end is cut; the browser shows the rest of a longer one.
const PAGE_HEIGHT: u32 = 6000;
/// What a served page runs: it asks every second when the file last
/// changed, and reloads once that is no longer when it was served.
fn reload(served: u128) -> String {
    format!(
        "<script>setInterval(async()=>{{try{{const r=await fetch('__changed',{{cache:'no-store'}});if(await r.text()!=='{served}')location.reload()}}catch(e){{}}}},1000)</script>"
    )
}

/// `sonny view ...`.
pub fn run(args: &[String]) -> Result<(), String> {
    match args {
        [serve, file] if serve == "--serve" => serve_file(Path::new(file)),
        [flag, file] if flag == "--watch" => watch(Path::new(file)),
        [file, browser] | [browser, file] if browser == "--browser" && !file.starts_with('-') => {
            open(&readable(Path::new(file))?)
        }
        [file] if !file.starts_with('-') => beside(&readable(Path::new(file))?),
        _ => Err(USAGE.into()),
    }
}

/// `file` by its whole path, once it is known to be a file.
fn readable(file: &Path) -> Result<PathBuf, String> {
    let file = std::fs::canonicalize(file).map_err(|e| format!("{}: {e}", file.display()))?;
    match file.is_file() {
        true => Ok(file),
        false => Err(format!("{} is not a file", file.display())),
    }
}

/// Show `file` in a pane beside the agent whose shell asked, else in the
/// browser; or here, in the person's own terminal.
fn beside(file: &Path) -> Result<(), String> {
    let Ok(owner) = std::env::var("AGENT_BOT") else {
        return watch(file);
    };
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let name = file
        .file_name()
        .map(|name| format!("view {}", name.to_string_lossy().replace('/', "-")))
        .unwrap_or_else(|| "view".into());
    let command: Vec<String> = [me.as_path(), Path::new("view"), Path::new("--watch"), file]
        .iter()
        .map(|part| part.to_string_lossy().into_owned())
        .collect();
    let dir = file.parent().unwrap_or(Path::new("/"));
    let placed = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?
        .block_on(async {
            let (_, _, mut layout) = crate::agents_layout(&owner, None).await?;
            layout.split(&owner, &name, dir, &command, false)
        });
    match placed {
        Ok(_) => {
            println!(
                "Shown beside you, and drawn again whenever {} changes.",
                file.display()
            );
            Ok(())
        }
        Err(why) => {
            println!("No pane to show it in ({why}), so it opens in the browser.");
            open(file)
        }
    }
}

/// The file on this terminal's whole screen until the person closes it,
/// drawn again whenever it changes or the screen's width does.
fn watch(file: &Path) -> Result<(), String> {
    let file = readable(file)?;
    enable_raw_mode().map_err(|e| e.to_string())?;
    crate::term::detect();
    let _ = execute!(std::io::stdout(), EnterAlternateScreen);
    let shown = Terminal::new(CrosstermBackend::new(std::io::stdout()))
        .map_err(|e| e.to_string())
        .and_then(|mut terminal| show(&mut terminal, &file));
    let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
    let _ = disable_raw_mode();
    shown
}

fn show(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    file: &Path,
) -> Result<(), String> {
    let name = file
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let _ = terminal.hide_cursor();
    let mut drawn: Option<(u128, u16)> = None;
    let mut lines = Vec::new();
    let mut top = 0;
    loop {
        let size = terminal.size().map_err(|e| e.to_string())?;
        let now = (changed(file), size.width);
        if drawn != Some(now) {
            lines = draw(file, size.width as usize);
            drawn = Some(now);
        }
        let height = size.height.saturating_sub(2) as usize;
        let last = lines.len().saturating_sub(height);
        top = top.min(last);
        let header = Line::from(vec![
            Span::raw(name.clone()).bold(),
            Span::raw(" · drawn again when it changes · q to close").dim(),
            Span::raw(match last {
                0 => String::new(),
                _ => format!(" · {}%", top * 100 / last),
            })
            .dim(),
        ]);
        let page: Vec<Line> = lines.iter().skip(top).take(height).cloned().collect();
        terminal
            .draw(|frame| {
                let area = frame.area();
                let (head, body) = (
                    ratatui::layout::Rect { height: 1, ..area },
                    ratatui::layout::Rect {
                        y: area.y + 2,
                        height: area.height.saturating_sub(2),
                        ..area
                    },
                );
                frame.render_widget(Paragraph::new(header), head);
                frame.render_widget(Paragraph::new(page), body);
            })
            .map_err(|e| e.to_string())?;
        if !event::poll(Duration::from_millis(500)).map_err(|e| e.to_string())? {
            continue;
        }
        let Event::Key(key) = event::read().map_err(|e| e.to_string())? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        top = match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
            KeyCode::Down | KeyCode::Char('j') => top + 1,
            KeyCode::Up | KeyCode::Char('k') => top.saturating_sub(1),
            KeyCode::PageDown | KeyCode::Char(' ') => top + height.max(2) - 1,
            KeyCode::PageUp | KeyCode::Char('b') => top.saturating_sub(height.max(2) - 1),
            KeyCode::Home | KeyCode::Char('g') => 0,
            KeyCode::End | KeyCode::Char('G') => last,
            _ => top,
        };
    }
}

/// The file as rows `width` wide: Markdown as a reply is drawn, an SVG or
/// Mermaid file as its picture, a page as its picture top to bottom, and
/// anything else as its text.
fn draw(file: &Path, width: usize) -> Vec<Line<'static>> {
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(error) => return vec![Line::from(format!("{}: {error}", file.display())).dim()],
    };
    let fenced = |lang: &str| markdown(&format!("```{lang}\n{}\n```", text.trim_end()), width);
    match file.extension().and_then(|e| e.to_str()) {
        Some("md" | "markdown") => markdown(&text, width),
        Some("svg") => fenced("svg"),
        Some("mmd" | "mermaid") => fenced("mermaid"),
        Some("html" | "htm") => picture(file, width),
        _ => text
            .lines()
            .flat_map(|line| {
                crate::transcript::wrap(Line::from(line.to_owned()), width, "".into(), "".into())
            })
            .collect(),
    }
}

/// Markdown as a reply is drawn, without a reply's bullet.
fn markdown(text: &str, width: usize) -> Vec<Line<'static>> {
    let mut markdown = crate::transcript::Markdown::default();
    let mut out: Vec<Line<'static>> = text
        .lines()
        .flat_map(|line| markdown.feed(line, width, false))
        .collect();
    out.extend(markdown.end(width, false));
    out
}

/// A page's picture, top to bottom, where the terminal can show pictures.
fn picture(file: &Path, width: usize) -> Vec<Line<'static>> {
    let browser = format!("sonny view {} --browser shows it", file.display());
    if !crate::term::graphics() {
        return vec![Line::from(format!("This terminal cannot show pictures; {browser}.")).dim()];
    }
    let drawn = crate::html::shoot(file, crate::image::page_width(width), PAGE_HEIGHT, 2)
        .and_then(|png| crate::image::tiles(&png, width));
    match drawn {
        Ok((tiles, cut)) => {
            let mut out = Vec::new();
            for tile in &tiles {
                match crate::image::show(tile) {
                    Ok(rows) => out.extend(rows),
                    Err(error) => return vec![Line::from(error).dim()],
                }
            }
            if cut {
                out.push(Line::from(format!("  The page goes on; {browser} whole.")).dim());
            }
            out
        }
        Err(error) => {
            vec![Line::from(format!("Could not draw the page ({error}); {browser}.")).dim()]
        }
    }
}

/// Start the server apart from this shell, and open the page it serves.
fn open(file: &Path) -> Result<(), String> {
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut server = Command::new(me);
    server
        .args(["view", "--serve"])
        .arg(file)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // Its own process group, so the shell's end does not end it.
    std::os::unix::process::CommandExt::process_group(&mut server, 0);
    let mut child = server.spawn().map_err(|e| e.to_string())?;
    let mut url = String::new();
    BufReader::new(child.stdout.take().ok_or("no server output")?)
        .read_line(&mut url)
        .map_err(|e| e.to_string())?;
    let url = url.trim().to_owned();
    if url.is_empty() {
        return Err("the page's server did not start".into());
    }
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let opened = Command::new(opener)
        .arg(&url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    match opened {
        true => println!("Opened {url}; the tab reloads whenever the file changes."),
        false => println!("Serving {url}; the tab reloads whenever the file changes."),
    }
    Ok(())
}

/// Serve `file`, and what sits beside it, at a path no other page can
/// guess, until no tab has asked for [`IDLE`].
fn serve_file(file: &Path) -> Result<(), String> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let token = token()?;
    let name = file
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    println!("http://127.0.0.1:{port}/{token}/{}", escape_path(&name));
    std::io::stdout().flush().map_err(|e| e.to_string())?;
    let asked = Arc::new(AtomicU64::new(now()));
    let watch = asked.clone();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(30));
            if now().saturating_sub(watch.load(Ordering::Relaxed)) > IDLE.as_secs() {
                std::process::exit(0);
            }
        }
    });
    let dir = file.parent().unwrap_or(Path::new("/")).to_owned();
    for stream in listener.incoming().flatten() {
        asked.store(now(), Ordering::Relaxed);
        let (file, dir, token) = (file.to_owned(), dir.clone(), token.clone());
        std::thread::spawn(move || {
            let _ = answer(stream, &file, &dir, &token);
        });
    }
    Ok(())
}

fn answer(mut stream: TcpStream, file: &Path, dir: &Path, token: &str) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut request = String::new();
    BufReader::new(stream.try_clone()?).read_line(&mut request)?;
    let path = request.split_whitespace().nth(1).unwrap_or("/");
    let path = path.split('?').next().unwrap_or(path);
    let (status, kind, body) = match respond(path, file, dir, token) {
        Some((kind, body)) => ("200 OK", kind, body),
        None => ("404 Not Found", "text/plain", b"not here".to_vec()),
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(&body)
}

/// What `path` asks for: when the file changed, the file as a page that
/// reloads itself, or a file beside it the page refers to.
fn respond(path: &str, file: &Path, dir: &Path, token: &str) -> Option<(&'static str, Vec<u8>)> {
    let rest = path
        .strip_prefix('/')?
        .strip_prefix(token)?
        .strip_prefix('/')?;
    if rest == "__changed" {
        return Some(("text/plain", changed(file).to_string().into_bytes()));
    }
    let name = unescape_path(rest)?;
    let asked = std::fs::canonicalize(dir.join(&name)).ok()?;
    if asked == file {
        return Some(("text/html; charset=utf-8", page(file)));
    }
    // Only what is in the file's folder, never above it.
    if !asked.starts_with(dir) || !asked.is_file() {
        return None;
    }
    Some((kind(&asked), std::fs::read(&asked).ok()?))
}

/// The file as a page: HTML as it is, anything else as its text; with the
/// script that reloads it.
fn page(file: &Path) -> Vec<u8> {
    let served = changed(file);
    let text = std::fs::read_to_string(file).unwrap_or_else(|e| format!("{}: {e}", file.display()));
    let html = matches!(
        file.extension().and_then(|e| e.to_str()),
        Some("html" | "htm" | "svg")
    );
    let mut page = match html {
        true => text,
        false => format!(
            "<!doctype html><meta charset=utf-8><pre style=\"white-space:pre-wrap\">{}</pre>",
            escape_html(&text)
        ),
    };
    page.push_str(&reload(served));
    page.into_bytes()
}

/// When `file` last changed, in nanoseconds; 0 when it cannot be read.
fn changed(file: &Path) -> u128 {
    std::fs::metadata(file)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |at| at.as_nanos())
}

fn kind(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("css") => "text/css",
        Some("js") => "text/javascript",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("json") => "application/json",
        _ => "text/plain; charset=utf-8",
    }
}

/// A path no other page can guess, from the system's randomness.
fn token() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut bytes))
        .map_err(|e| e.to_string())?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A file name as a URL path, every byte that is not plainly safe escaped.
fn escape_path(name: &str) -> String {
    name.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn unescape_path(path: &str) -> Option<PathBuf> {
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            let hex = std::str::from_utf8(bytes.get(at + 1..at + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    Some(PathBuf::from(String::from_utf8(out).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_is_drawn_by_its_kind() {
        let dir = std::env::temp_dir().join(format!("sonny-view-kinds-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let drawn = |name: &str, text: &str| -> Vec<String> {
            let file = dir.join(name);
            std::fs::write(&file, text).unwrap();
            draw(&file, 40).iter().map(|row| row.to_string()).collect()
        };
        let notes = drawn("notes.md", "# Findings\n\n- **one** thing\n");
        assert!(
            notes.iter().any(|row| row.contains("Findings")),
            "{notes:?}"
        );
        assert!(
            notes
                .iter()
                .all(|row| !row.contains("**") && !row.starts_with("• "))
        );
        assert_eq!(
            drawn("log.txt", "first\nsecond"),
            ["first", "second"],
            "anything else is its text"
        );
        // Where pictures cannot be shown, the page points to the browser.
        let page = drawn("report.html", "<h1>Report</h1>");
        assert!(page[0].contains("--browser"), "{page:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_page_reloads_itself_and_nothing_outside_its_folder_is_served() {
        let dir = std::env::temp_dir().join(format!("sonny-view-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("img")).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let file = dir.join("report one.html");
        std::fs::write(&file, "<h1>Report</h1>").unwrap();
        std::fs::write(dir.join("img/a.png"), b"png").unwrap();
        let ask = |path: &str| respond(path, &file, &dir, "t0k");
        let (kind, body) = ask("/t0k/report%20one.html").unwrap();
        assert_eq!(kind, "text/html; charset=utf-8");
        let body = String::from_utf8(body).unwrap();
        assert!(body.starts_with("<h1>Report</h1>") && body.ends_with("</script>"));
        assert_eq!(ask("/t0k/img/a.png"), Some(("image/png", b"png".to_vec())));
        assert_eq!(
            ask("/t0k/__changed").unwrap().1,
            changed(&file).to_string().into_bytes()
        );
        for refused in [
            "/other/report%20one.html",
            "/t0k/../etc/passwd",
            "/t0k/%2e%2e/x",
            "/",
        ] {
            assert_eq!(ask(refused), None, "{refused}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn text_is_shown_as_text() {
        let dir = std::env::temp_dir().join(format!("sonny-view-text-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("notes.md");
        std::fs::write(&file, "# a <b>").unwrap();
        let page = String::from_utf8(page(&file)).unwrap();
        assert!(page.contains("<pre style=\"white-space:pre-wrap\"># a &lt;b&gt;</pre>"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
