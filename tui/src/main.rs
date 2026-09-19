//! `agent-tui`: a terminal client for a running agent daemon. Open it and you
//! are talking to a bot; the rest of the fleet is one key away. Detaching
//! leaves the daemon and its bots running; attaching resumes from the last
//! event cursor this client saw.
mod app;
mod items;
use agent_client as client;
mod session;
mod ui;

use app::{App, Picker};
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent, KeyModifiers,
    MouseEventKind,
};
use futures_util::StreamExt;
use std::{path::PathBuf, time::Duration};

const USAGE: &str = "usage: agent-tui [--socket PATH | --store PATH] [--model PROVIDER/MODEL] [--workspace DIR] [--no-motion]

Attach to a running agent daemon. Socket selection: --socket, then
AGENT_SOCKET, then the socket adjacent to --store, AGENT_STORE, or
~/.agent/state.sqlite. New bots use --model or AGENT_MODEL and --workspace
or the current directory. --no-motion snaps panes instead of sliding them.
AGENT_TUI_THEME=light|dark|none overrides the terminal's theme answer.";

struct Args {
    socket: PathBuf,
    model: Option<String>,
    workspace: String,
    motion: bool,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let (mut socket, mut store, mut model, mut workspace, mut motion) =
        (None, None, None, None, true);
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) => (flag, Some(value.to_owned())),
            None => (arg.as_str(), None),
        };
        let mut value = || {
            inline
                .clone()
                .or_else(|| iter.next().cloned())
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match flag {
            "--socket" => socket = Some(PathBuf::from(value()?)),
            "--store" => store = Some(PathBuf::from(value()?)),
            "--model" => model = Some(value()?),
            "--workspace" => workspace = Some(value()?),
            "--no-motion" => motion = false,
            "-h" | "--help" => return Err(USAGE.to_owned()),
            other => return Err(format!("unknown argument {other}\n{USAGE}")),
        }
    }
    let socket = match socket {
        Some(socket) => socket,
        None => match (store, std::env::var_os("AGENT_SOCKET")) {
            (None, Some(env)) => PathBuf::from(env),
            (store, _) => {
                let store = store
                    .or_else(|| std::env::var_os("AGENT_STORE").map(PathBuf::from))
                    .or_else(|| {
                        std::env::var_os("HOME")
                            .map(|home| PathBuf::from(home).join(".agent/state.sqlite"))
                    })
                    .ok_or("no store path; pass --socket or --store")?;
                let canonical = std::fs::canonicalize(&store).unwrap_or(store);
                let mut adjacent = canonical.into_os_string();
                adjacent.push(".sock");
                PathBuf::from(adjacent)
            }
        },
    };
    let workspace = match workspace {
        Some(dir) => dir,
        None => std::env::current_dir()
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .into_owned(),
    };
    Ok(Args {
        socket,
        model: model.or_else(|| std::env::var("AGENT_MODEL").ok()),
        workspace,
        motion,
    })
}

/// Rows for the switcher: (name, tree prefix, hint). Unfiltered shows the
/// creation tree; a query flattens to matches with the creator as the hint.
pub fn picker_rows(app: &App) -> Vec<(String, String, String)> {
    let query = app
        .ui
        .picker
        .as_ref()
        .map(|p| p.query.to_lowercase())
        .unwrap_or_default();
    app.tree()
        .into_iter()
        .filter(|(b, ..)| query.is_empty() || b.name.to_lowercase().contains(&query))
        .map(|(b, depth, last, trail)| {
            let state = ui::label(&b.status).to_owned();
            if query.is_empty() {
                let mut prefix = String::new();
                if depth > 0 {
                    for l in trail.iter().skip(1) {
                        prefix.push_str(if *l { "  " } else { "│ " });
                    }
                    prefix.push_str(if last { "└ " } else { "├ " });
                }
                (b.name.clone(), prefix, state)
            } else {
                let hint = [
                    b.parent
                        .as_ref()
                        .map(|p| format!("↳ {p}"))
                        .unwrap_or_default(),
                    state,
                ]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" · ");
                (b.name.clone(), String::new(), hint)
            }
        })
        .collect()
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args = match parse(&args) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(if message == USAGE { 0 } else { 2 });
        }
    };
    let mut app = App::new(args.socket, args.model, args.workspace, args.motion);
    app.compose_instructions();
    // One OSC query, answered by nearly every terminal: light or dark ground.
    // It decides only which near-background gray shades code blocks.
    // AGENT_TUI_THEME=light|dark|none overrides for terminals that do not answer.
    let theme = match std::env::var("AGENT_TUI_THEME").ok().as_deref() {
        Some("light") => Some(true),
        Some("dark") => Some(false),
        Some("none") => None,
        _ => {
            match terminal_colorsaurus::theme_mode(terminal_colorsaurus::QueryOptions::default()) {
                Ok(terminal_colorsaurus::ThemeMode::Light) => Some(true),
                Ok(terminal_colorsaurus::ThemeMode::Dark) => Some(false),
                Err(_) => None,
            }
        }
    };
    app.ui.shade = theme.map(|light| ratatui::style::Color::Indexed(if light { 255 } else { 236 }));
    let saved = session::load(&app.socket, &app.default_workspace);
    let mut events = match app.attach().await {
        Ok(events) => events,
        Err(error) => {
            eprintln!("agent-tui: {error}");
            std::process::exit(1);
        }
    };
    app.restore(&saved);
    app.load_visible().await;

    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
        ratatui::restore();
        hook(info);
    }));
    let mut terminal = ratatui::init();
    // The wheel scrolls the transcript; without capture the terminal would
    // scroll its own, empty, alternate-screen scrollback instead.
    let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture);
    let mut keys = EventStream::new();
    let mut dirty = true;
    let mut closed: Option<String> = None;
    loop {
        if dirty {
            if let Err(error) = terminal.draw(|frame| ui::render(frame, &app)) {
                let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
                ratatui::restore();
                eprintln!("agent-tui: draw failed: {error}");
                return;
            }
            dirty = false;
        }
        // Redraw only when something moves: a slide at 60 fps, a pulse or a
        // running counter at 2 Hz, a toast until it expires. Idle costs nothing.
        let tick = if app.animating() {
            Duration::from_millis(16)
        } else if app.busy() || app.ui.toast.is_some() {
            Duration::from_millis(500)
        } else {
            Duration::from_secs(3600)
        };
        tokio::select! {
            key = keys.next() => {
                match key {
                    Some(Ok(Event::Key(key))) => {
                        if key.kind == crossterm::event::KeyEventKind::Release { continue; }
                        if !handle_key(&mut app, key).await { break; }
                    }
                    Some(Ok(Event::Mouse(mouse))) => {
                        let width = terminal.size().map(|s| s.width).unwrap_or(0);
                        match mouse.kind {
                            MouseEventKind::ScrollUp => app.scroll_by(3, mouse.column, width),
                            MouseEventKind::ScrollDown => app.scroll_by(-3, mouse.column, width),
                            _ => continue,
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                }
                dirty = true;
            }
            event = events.recv() => {
                match event {
                    Some(event) => {
                        app.event(event).await;
                        while let Ok(event) = events.try_recv() { app.event(event).await; }
                        if app.reattach {
                            app.reattach = false;
                            match app.attach().await {
                                Ok(receiver) => events = receiver,
                                Err(error) => { closed = Some(format!("{error}")); break; }
                            }
                        }
                        app.load_visible().await;
                    }
                    None => { closed = Some("the daemon closed the session".into()); break; }
                }
                dirty = true;
            }
            _ = tokio::time::sleep(tick) => {
                if let Some((_, at)) = &app.ui.toast && at.elapsed() >= app::TOAST { app.ui.toast = None; }
                dirty = true;
            }
        }
    }
    session::save(&app.socket, &app.default_workspace, &app.session());
    let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    if let Some(why) = closed {
        eprintln!("agent-tui: {why}; the daemon keeps your bots, run again to attach");
    }
}

/// Returns false to quit. Quitting is detaching: the daemon keeps running and
/// the next start in this workspace resumes the same bot and panes.
async fn handle_key(app: &mut App, key: KeyEvent) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let k = key.code;
    if ctrl && k == KeyCode::Char('c') {
        return false;
    }
    if app.ui.help {
        app.ui.help = false;
        return true;
    }
    if app.ui.picker.is_some() {
        let rows = picker_rows(app);
        let p = app.ui.picker.as_mut().unwrap();
        match k {
            KeyCode::Esc => app.ui.picker = None,
            KeyCode::Enter => {
                let choice = rows.get(p.sel).map(|r| r.0.clone());
                app.ui.picker = None;
                if let Some(name) = choice {
                    app.select(&name);
                    app.load_visible().await;
                }
            }
            KeyCode::Down => p.sel = (p.sel + 1).min(rows.len().saturating_sub(1)),
            KeyCode::Up => p.sel = p.sel.saturating_sub(1),
            KeyCode::Char('n') if ctrl => p.sel = (p.sel + 1).min(rows.len().saturating_sub(1)),
            KeyCode::Char('p') if ctrl => p.sel = p.sel.saturating_sub(1),
            KeyCode::Backspace => {
                p.query.pop();
                p.sel = 0;
            }
            KeyCode::Char(c) if !ctrl => {
                p.query.push(c);
                p.sel = 0;
            }
            _ => {}
        }
        return true;
    }
    match (ctrl, k) {
        (true, KeyCode::Char('k')) => app.ui.picker = Some(Picker::default()),
        (true, KeyCode::Char('b')) => app.toggle_rail(),
        (true, KeyCode::Char('d')) => return false,
        (true, KeyCode::Char('t')) => app.ui.thoughts = !app.ui.thoughts,
        (true, KeyCode::Char('o')) => app.ui.output = !app.ui.output,
        (true, KeyCode::Char('u')) => app.input.clear(),
        (true, KeyCode::Char('p')) => {
            let peers = app.peers();
            if !peers.is_empty() {
                let i = app
                    .ui
                    .peek
                    .as_ref()
                    .and_then(|p| peers.iter().position(|x| x == p))
                    .map(|i| i + 1)
                    .unwrap_or(0);
                app.open_peek(&peers[i % peers.len()]);
                app.load_visible().await;
            }
        }
        (_, KeyCode::Esc) => {
            if app.ui.peek.is_some() {
                app.close_peek();
            } else if app.ui.rail.target() > 0 {
                app.toggle_rail();
            } else if let Err(e) = app.interrupt().await
                && e.code != "idle"
            {
                app.toast(format!("interrupt: {e}"));
            }
        }
        (_, KeyCode::Up | KeyCode::Down) if app.input.is_empty() => {
            let names: Vec<String> = app
                .tree()
                .into_iter()
                .map(|(b, ..)| b.name.clone())
                .collect();
            if let Some(i) = names.iter().position(|n| *n == app.selected) {
                let next = if k == KeyCode::Down {
                    (i + 1) % names.len()
                } else {
                    (i + names.len() - 1) % names.len()
                };
                app.select(&names[next]);
                app.load_visible().await;
            }
        }
        (_, KeyCode::PageUp) => app.ui.scroll = app.ui.scroll.saturating_add(10),
        (_, KeyCode::PageDown) => app.ui.scroll = app.ui.scroll.saturating_sub(10),
        (_, KeyCode::End) => app.ui.scroll = 0,
        (_, KeyCode::Enter) => {
            let text = std::mem::take(&mut app.input);
            let text = text.trim().to_owned();
            if text.is_empty() {
                return true;
            }
            let outcome = if let Some(spec) = text.strip_prefix("/new ") {
                app.create(spec.trim()).await
            } else if text == "/mouse" {
                app.ui.mouse = !app.ui.mouse;
                let _ = if app.ui.mouse {
                    crossterm::execute!(std::io::stdout(), EnableMouseCapture)
                } else {
                    crossterm::execute!(std::io::stdout(), DisableMouseCapture)
                };
                app.toast(if app.ui.mouse {
                    "mouse captured: wheel scrolls, shift-drag selects"
                } else {
                    "mouse released: drag selects, PgUp/PgDn scroll"
                });
                Ok(())
            } else if text == "/help" || text == "?" {
                app.ui.help = true;
                Ok(())
            } else {
                app.submit(text.clone()).await
            };
            if let Err(error) = outcome {
                app.toast(format!("{error}"));
                app.input = text;
            }
        }
        (_, KeyCode::Backspace) => {
            app.input.pop();
        }
        (false, KeyCode::Char('?')) if app.input.is_empty() => app.ui.help = true,
        (false, KeyCode::Char(c)) => app.input.push(c),
        _ => {}
    }
    true
}
