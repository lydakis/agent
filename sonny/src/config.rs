//! `/config`: the person's `~/.agent/config.toml`, read and changed from
//! Sonny, every key the file takes in one list. A key with a few values
//! steps through them, and one that takes a name is typed; either is
//! written to the file at once, the person's comments and order kept, and
//! a key taken back to its default has its line taken out.
use ratatui::crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    style::{Color, Stylize},
    text::{Line, Span},
};
use std::path::PathBuf;
use unicode_width::UnicodeWidthStr;

/// One key of the file.
pub struct Setting {
    /// As the file names it: `table.key`, or the bare key.
    pub key: &'static str,
    /// The group `/config` lists it under, as [`GROUPS`] names them; none
    /// for one that only tunes, which stays in the file and the reference.
    pub group: &'static str,
    /// What `/config` calls it, in plain words.
    pub name: &'static str,
    /// What it does, in one line `/config` shows: at most the Rulebook's
    /// `label-max`, 70 characters.
    pub label: &'static str,
    /// What it sets and when that is read, whole, for the reference.
    pub about: &'static str,
    /// The values it takes, or none for one that takes a name.
    pub values: &'static [&'static str],
    /// What an unset key is.
    pub default: &'static str,
}

/// `/config`'s groups, in the order it lists them.
pub const GROUPS: &[&str] = &[
    "Models",
    "Agents",
    "View",
    "Margin",
    "Tabs",
    "Commons and sky",
    "This machine",
];

const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];
/// A switch, written as TOML's own true or false.
const SWITCH: &[&str] = &["true", "false"];
/// What `trailer` lists, each written as a TOML list of its words.
const TRAILERS: &[&str] = &["sender, time", "sender", "time", "none"];

/// Whether a setting takes a whole number, typed: its default is one.
fn number(setting: &Setting) -> bool {
    setting.values.is_empty() && setting.default.parse::<u64>().is_ok()
}

/// Whether a setting is a span of time, as `30s`, `15m` or `1h`.
fn span(setting: &Setting) -> bool {
    setting.values.is_empty()
        && !number(setting)
        && agent_client::config::span(setting.key, setting.default).is_ok()
}

/// The rows `/config` takes: the blank one and a list's rows.
pub fn height() -> usize {
    1 + crate::list::ROWS
}

/// Every key `agent_client::config` reads: those `/config` lists in its
/// groups' order, then those that only tune.
pub const SETTINGS: &[Setting] = &[
    Setting {
        key: "model",
        group: "Models",
        name: "model",
        label: "the model a new agent runs when none is named",
        about: "a new agent's model, when none is named (provider/model)",
        values: &[],
        default: "none",
    },
    Setting {
        key: "provider",
        group: "Models",
        name: "provider",
        label: "the providers the daemon serves; read when it starts",
        about: "the daemon's providers, as AGENT_PROVIDER takes them; read when it starts",
        values: &[],
        default: "AGENT_PROVIDER",
    },
    Setting {
        key: "sonny.lead_effort",
        group: "Models",
        name: "lead effort",
        label: "how hard a new lead thinks",
        about: "a new lead's effort",
        values: EFFORTS,
        default: "the model's own",
    },
    Setting {
        key: "sonny.task_effort",
        group: "Models",
        name: "task effort",
        label: "how hard each new task thinks",
        about: "each new task's effort",
        values: EFFORTS,
        default: "its lead's",
    },
    Setting {
        key: "sonny.task_speed",
        group: "Models",
        name: "task speed",
        label: "each new task's speed, where its model serves one",
        about: "each new task's speed, where its model serves it; unset, its starter's when on the same model",
        values: &agent_client::config::SPEEDS,
        default: "its starter's",
    },
    Setting {
        key: "reviewer.model",
        group: "Models",
        name: "reviewer model",
        label: "the model that reviews calls in Auto",
        about: "the model that reviews calls in Auto (provider/model); unset, the agent's own model",
        values: &[],
        default: "the agent's own",
    },
    Setting {
        key: "reviewer.effort",
        group: "Models",
        name: "reviewer effort",
        label: "how hard the reviewer thinks",
        about: "the reviewer's effort",
        values: EFFORTS,
        default: "the model's own",
    },
    Setting {
        key: "reviewer.timeout_ms",
        group: "Models",
        name: "reviewer wait",
        label: "how long a call in Auto waits for the reviewer before it is denied",
        about: "how long a call in Auto waits for the reviewer's verdict, every attempt included, before it is denied as not reviewed and the agent told so; 0 waits as long as it takes",
        values: &[],
        default: "90000",
    },
    Setting {
        key: "sonny.default_permissions",
        group: "Agents",
        name: "permissions",
        label: "the /permissions mode a new lead starts in",
        about: "the /permissions mode a new lead starts in; a [sonny.permissions] mode of your own is typed into the file",
        values: &["auto", "ask", "read-only", "full"],
        default: "auto",
    },
    Setting {
        key: "sonny.lead_profile",
        group: "Agents",
        name: "lead profile",
        label: "the profile a new lead runs as",
        about: "the profile a new lead runs as",
        values: &[],
        default: "lead",
    },
    Setting {
        key: "sonny.task_profile",
        group: "Agents",
        name: "task profile",
        label: "the profile every other agent Sonny starts runs as",
        about: "the profile every other agent Sonny starts is told",
        values: &[],
        default: "task",
    },
    Setting {
        key: "sonny.task_worktrees",
        group: "Agents",
        name: "task worktrees",
        label: "task folders kept; past it a new task removes the least active",
        about: "task folders kept in ~/.agent/worktrees, as Codex's app keeps its newest worktrees: past it, a new task's start removes the least recently active, as remove_task does (marked done, branch kept while it has commits not merged, agent and history kept), never one working, needing you, with uncommitted changes or submodules, of its own, or another daemon's; at least 1",
        values: &[],
        default: "15",
    },
    Setting {
        key: "sonny.side_chat",
        group: "Agents",
        name: "side chat",
        label: "/side's copy can do anything its agent can, or only answers",
        about: "/side's copy can do anything its agent can, with its tools, in its folder, under its approvals; or answers from what its agent knows, calling no tool",
        values: &["work", "answer"],
        default: "work",
    },
    Setting {
        key: "sonny.away_minutes",
        group: "Agents",
        name: "away",
        label: "minutes without you before leads hold news for your return; 0 never",
        about: "minutes without your use of any window, or a message through sonny acp, before every lead is told you are away; it puts only decisions to you and tells the rest in one message once you are back; 0 never",
        values: &[],
        default: "30",
    },
    Setting {
        key: "sonny.ask_wait_minutes",
        group: "Agents",
        name: "ask wait",
        label: "minutes a question waits before its agent goes on; 0 goes on at once",
        about: "minutes an unanswered question waits before its agent goes on with what it recommended, where it said it can be undone, or is reminded of a one-way door; 0 goes on at once where it can be undone and never reminds",
        values: &[],
        default: "60",
    },
    Setting {
        key: "web_search",
        group: "Agents",
        name: "web search",
        label: "whether new agents search and fetch the web",
        about: "whether new agents search and fetch the web where their provider offers it",
        values: &["live", "disabled"],
        default: "live",
    },
    Setting {
        key: "sonny.code_mode",
        group: "Agents",
        name: "code mode",
        label: "whether agents run their shell and file tools from scripts",
        about: "whether agents reach their shell and file tools only from exec scripts, which run them as the same calls would, under the same rules; off, they call them directly again; from each agent's next turn",
        values: SWITCH,
        default: "true",
    },
    Setting {
        key: "trailer",
        group: "Agents",
        name: "senders",
        label: "what an agent reads after each message: who sent it, and when",
        about: "what an agent's model reads after each message: who sent it, when it was sent",
        values: TRAILERS,
        default: "sender, time",
    },
    Setting {
        key: "daemon.depth",
        group: "Agents",
        name: "depth",
        label: "how many generations of agents a line may have; 0 for no end",
        about: "how many generations below the agent that starts a line (a lead, or an agent you made) a new agent may be: 2 lets a lead's tasks start their own, which start none; 0 for no end; read when the daemon starts",
        values: &[],
        default: "2",
    },
    Setting {
        key: "sonny.view",
        group: "View",
        name: "view",
        label: "what the conversation shows: words, steps, or every tool call",
        about: "what the conversation shows: `words`, only your words and the agent's, its questions and what failed; `steps`, the agent's steps between too (calls, news in, messages between agents, how long a turn took); `verbose`, every tool call. `/verbose` shows every tool call in one window until it is typed again",
        values: &["words", "steps", "verbose"],
        default: "words",
    },
    Setting {
        key: "sonny.text_width",
        group: "View",
        name: "text width",
        label: "the widest the conversation is written, in columns; 0 for all",
        about: "the widest the conversation is written, in columns, however wide the window, the pane beside it taking the rest; 0 for the window's whole width, shared half and half with an open pane",
        values: &[],
        default: "100",
    },
    Setting {
        key: "sonny.project_colors",
        group: "View",
        name: "project colors",
        label: "experimental: a followed project's replies wear its color",
        about: "experimental: in a window that follows other leads, a reply about one followed project wears its color",
        values: SWITCH,
        default: "false",
    },
    Setting {
        key: "sonny.suggest",
        group: "View",
        name: "suggest",
        label: "after your turn, guess your next message for Tab to take",
        about: "after a turn you started, a copy of the agent guesses your next message for Tab to take; one small call",
        values: SWITCH,
        default: "true",
    },
    Setting {
        key: "sonny.suggest_ideas",
        group: "View",
        name: "suggest ideas",
        label: "when the next message isn't clear, the guess may offer an idea",
        about: "when your next message isn't obvious, that guess may offer an idea for the work or a tip for a Sonny command or key instead, labeled; no extra call",
        values: SWITCH,
        default: "true",
    },
    Setting {
        key: "sonny.chime",
        group: "View",
        name: "chime",
        label: "ring when work finishes or waits; notifications show either way",
        about: "whether finished and waiting work rings; notifications show either way",
        values: SWITCH,
        default: "false",
    },
    Setting {
        key: "sonny.margin_opens",
        group: "Margin",
        name: "opens",
        label: "when a note the agent puts up opens the margin",
        about: "when a note the agent puts up or changes opens the margin: always, unless you closed it with Esc, or never",
        values: &["always", "unless-closed", "never"],
        default: "always",
    },
    Setting {
        key: "sonny.margin_notes",
        group: "Margin",
        name: "notes",
        label: "the most notes a margin holds",
        about: "the most notes a margin holds; past it the agent's new note is refused, and it is told why",
        values: &[],
        default: "6",
    },
    Setting {
        key: "sonny.margin_note_lines",
        group: "Margin",
        name: "note lines",
        label: "the most rows a margin note draws",
        about: "the most rows a margin note draws, counted at the margin's own width, so the same in every window; a longer note is refused, and a file or a task's note folds at it",
        values: &[],
        default: "40",
    },
    Setting {
        key: "sonny.agent_opens",
        group: "Tabs",
        name: "agent opens",
        label: "where a click on an agent's name takes you: its tab, or beside",
        about: "where a click on an agent's name takes you: its tab in Rex, Herdr or cmux, or a look beside the conversation; a right click, the other",
        values: &["tab", "beside"],
        default: "tab",
    },
    Setting {
        key: "sonny.task_tabs",
        group: "Tabs",
        name: "task tabs",
        label: "which tasks open a tab of their own: those asked for, or every one",
        about: "which tasks open in a tab of their own in Rex, Herdr or cmux: those an agent shows you or you open; or every task as its turn starts",
        values: &["asked", "every"],
        default: "asked",
    },
    Setting {
        key: "sonny.most_task_tabs",
        group: "Tabs",
        name: "most task tabs",
        label: "the most task tabs a lead keeps open; 0 for no limit",
        about: "task tabs a lead keeps open in its multiplexer: past it, a new one closes the one you used least recently, never one you have not looked at yet, that needs you, failed or runs something it asked for (with only unseen ones to close, the new one waits for /status); 0 for no limit",
        values: &[],
        default: "32",
    },
    Setting {
        key: "sonny.commons",
        group: "Commons and sky",
        name: "commons",
        label: "whether agents have the commons; read when Sonny starts",
        about: "whether agents have the commons: its tool, its skill and the roles' lines about it; read when Sonny starts",
        values: SWITCH,
        default: "true",
    },
    Setting {
        key: "sonny.boards",
        group: "Commons and sky",
        name: "boards",
        label: "whether agents have boards; read when Sonny starts",
        about: "whether agents have boards, work in the commons they take: the commons tool's work fields, the commons skill's Boards section, and start_task's board and agents; only with the commons; read when Sonny starts",
        values: SWITCH,
        default: "true",
    },
    Setting {
        key: "sonny.hold_quiet",
        group: "",
        name: "hold quiet",
        label: "",
        about: "minutes work an agent took may go without a post in its thread before the holder is asked whether it is still on it and its board hears it went quiet; 0 never; read as work is taken",
        values: &[],
        default: "15",
    },
    Setting {
        key: "sonny.commons_invites",
        group: "Commons and sky",
        name: "commons invites",
        label: "the fewest commons threads a day that send invites; 0 for none",
        about: "least commons threads a day that send invites, the square root of the week's agents if more, 0 for none; each answered thread adds one, up to as many again",
        values: &[],
        default: "3",
    },
    Setting {
        key: "sonny.commons_pointer",
        group: "Commons and sky",
        name: "commons pointer",
        label: "whether a new task's brief names commons threads sharing its words",
        about: "whether a new task's brief ends with a line naming up to three commons threads whose titles share at least two of its words, the rarer words weighing more; read when Sonny starts",
        values: SWITCH,
        default: "true",
    },
    Setting {
        key: "sonny.sources.commons.digest",
        group: "Commons and sky",
        name: "commons digest",
        label: "how often at most an agent hears of new posts in its threads",
        about: "how often at most an agent hears of new posts in the commons threads it posted in, as 30s, 15m or 1h; 0 for as they come. The commons is a source, so steer sets what cuts in, as for yours",
        values: &[],
        default: "15m",
    },
    Setting {
        key: "sonny.sky_haikus",
        group: "Commons and sky",
        name: "sky haikus",
        label: "a haiku from the sky's busiest agent each time the title passes",
        about: "a haiku from the agent the sky finds busiest, written by a copy of it that calls no tool each time the title passes",
        values: SWITCH,
        default: "true",
    },
    Setting {
        key: "sonny.start_at_boot",
        group: "This machine",
        name: "start at boot",
        label: "start the daemon when this machine starts, at login on a Mac",
        about: "the daemon starts when this machine does (at login on a Mac), so a box's agents keep working after a restart",
        values: SWITCH,
        default: "false",
    },
    Setting {
        key: "sonny.sky_haiku_daily",
        group: "",
        name: "sky haiku daily",
        label: "",
        about: "the most haikus a day; 0 for no limit, the title's hold keeping them a hold apart",
        values: &[],
        default: "0",
    },
    Setting {
        key: "sonny.sky_haiku_keep",
        group: "",
        name: "sky haiku keep",
        label: "",
        about: "the hours haikus are kept, for a replay to show the one up at its time; 0 keeps them all",
        values: &[],
        default: "24",
    },
    Setting {
        key: "sonny.sky_haiku_window",
        group: "",
        name: "sky haiku window",
        label: "",
        about: "the minutes of messages and tool calls that make an agent the sky's busiest",
        values: &[],
        default: "10",
    },
    Setting {
        key: "sonny.sky_haiku_margin",
        group: "",
        name: "sky haiku margin",
        label: "",
        about: "how much busier, in percent, an agent must be than the busiest to take the title, so it does not flicker",
        values: &[],
        default: "50",
    },
    Setting {
        key: "sonny.sky_haiku_hold",
        group: "",
        name: "sky haiku hold",
        label: "",
        about: "the minutes the busiest keeps the title before another can take it",
        values: &[],
        default: "10",
    },
    Setting {
        key: "sonny.commons_invite_tokens",
        group: "",
        name: "commons invite tokens",
        label: "",
        about: "the tokens of work an invite pays for: the budget of the one turn it starts, a golden ticket's twice that; later replies reach the agent through its follow",
        values: &[],
        default: "1000000",
    },
    Setting {
        key: "sonny.tool_processes",
        group: "",
        name: "tool processes",
        label: "",
        about: "processes Sonny runs for agents' tool calls at once in all, each agent at most half; 0 for four times the CPUs",
        values: &[],
        default: "0",
    },
    Setting {
        key: "sonny.lookout_waiting",
        group: "",
        name: "lookout waiting",
        label: "",
        about: "events from your sources that wait for one agent while it is busy: past it, newer ones are not kept, and the agent's next note says how many, from where and when; 0 for no limit",
        values: &[],
        default: "1024",
    },
    Setting {
        key: "sonny.lookout_trailer",
        group: "Agents",
        name: "lookout trailer",
        label: "what ends every lookout note: when to reply to it",
        about: "what ends every note the lookout sends an agent, under a `[lookout]` header, read as each note goes; by default when to reply: when the news bears on what the person is doing now or is urgent, and otherwise an empty reply; `none` for none",
        values: &[],
        default: "when to reply",
    },
];

/// The migration that moves `[sonny] task_depth` to `[daemon] depth`.
pub const DEPTH: &str = "task-depth-daemon";
/// The migration that renames `[judge]` to `[reviewer]`.
pub const REVIEWER: &str = "judge-reviewer";
/// The migration that takes quiet mode's keys, files and skill away.
pub const QUIET_GONE: &str = "quiet-gone";
pub const CHECKS_GONE: &str = "checks-gone";
/// The migration that makes `[sonny] steps` and `verbose` one `view`.
pub const VIEW_ONE: &str = "view-one";
/// The migration that renames `[sonny] task_windows` `most_task_tabs`,
/// and `side_chat = "fork"` `"work"`, so the file says the Rulebook's
/// words (0.10.0).
pub const TAB_WORDS: &str = "tab-words";

/// At start, once each, before the config is read: a line depth the
/// person set for Sonny moved to the daemon, which bounds it now; the
/// reviewer's model and effort under the reviewer's name; quiet mode,
/// gone, leaving no key the config would refuse, no file and no skill; and
/// the reply check's and done check's keys, gone with the checks; and the
/// two switches for what the conversation shows, one `view`; and the
/// words its keys and values say, the Rulebook's.
pub fn at_start() {
    let (Some(home), Some(path)) = (
        std::env::var_os("HOME").map(PathBuf::from),
        agent_client::config::path(),
    ) else {
        return;
    };
    // No config, nothing to move: and nothing made, not even a HOME gone.
    if !path.exists() {
        return;
    }
    let migrated = home.join(".agent/sonny/migrated");
    crate::upgrade::once(&migrated, DEPTH, || rewrite(&path, depth_moved));
    crate::upgrade::once(&migrated, REVIEWER, || rewrite(&path, reviewer_named));
    crate::upgrade::once(&migrated, QUIET_GONE, || {
        for file in ["away", "present"] {
            let _ = std::fs::remove_file(home.join(".agent/sonny").join(file));
        }
        crate::skills::remove("away");
        rewrite(&path, quiet_gone)
    });
    crate::upgrade::once(&migrated, CHECKS_GONE, || rewrite(&path, checks_gone));
    crate::upgrade::once(&migrated, VIEW_ONE, || rewrite(&path, view_one));
    crate::upgrade::once(&migrated, TAB_WORDS, || rewrite(&path, tab_words));
}

/// The config at `path` as `change` rewrites it, each other line as the
/// person wrote it. Whether it is done: one it can't change cleanly is
/// left as it is, for the config's own error to say where the key went,
/// and tried again next time.
fn rewrite(path: &std::path::Path, change: fn(&str) -> Result<Option<String>, ()>) -> bool {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => return e.kind() == std::io::ErrorKind::NotFound,
    };
    match change(&text) {
        Err(()) => false,
        Ok(None) => true,
        Ok(Some(moved)) => {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path).map_or(0o600, |m| m.permissions().mode() & 0o777);
            agent_client::files::replace_mode(path, moved.as_bytes(), mode).is_ok()
        }
    }
}

/// `text` as [`rewrite`] writes it for [`DEPTH`]: none when `[sonny]` sets no
/// `task_depth`. Its line goes, and `depth` joins `[daemon]`, made at the
/// end when there is none, unless the file sets it there already. What is
/// written must parse to the config with only that key moved, so a key
/// lines can't move (a dotted key, an inline table) is an error.
fn depth_moved(text: &str) -> Result<Option<String>, ()> {
    use toml::Value;
    let old: toml::Table = text.parse().map_err(drop)?;
    let depth = (old.get("sonny").and_then(Value::as_table))
        .and_then(|sonny| sonny.get("task_depth"))
        .and_then(Value::as_integer);
    let Some(depth) = depth else {
        return Ok(None);
    };
    let mut expected = old.clone();
    let Some(Value::Table(sonny)) = expected.get_mut("sonny") else {
        return Err(());
    };
    sonny.remove("task_depth");
    let daemon = expected
        .entry("daemon")
        .or_insert_with(|| Value::Table(toml::Table::new()));
    let Value::Table(daemon) = daemon else {
        return Err(());
    };
    let add = !daemon.contains_key("depth");
    if add {
        daemon.insert("depth".into(), Value::Integer(depth));
    }
    let line = format!("depth = {depth}");
    let (mut table, mut removed, mut added) = (String::new(), false, !add);
    let mut lines = Vec::new();
    for raw in text.lines() {
        let trimmed = raw.trim();
        // A heading, `[sonny]`, a comment after it or not.
        if let Some(heading) = trimmed
            .strip_prefix('[')
            .filter(|rest| !rest.starts_with('['))
            && let Some((name, _)) = heading.split_once(']')
        {
            table = name.trim().to_owned();
        }
        let key = trimmed.split_once('=').map(|(key, _)| key.trim());
        if table == "sonny" && key == Some("task_depth") && !removed {
            removed = true;
            continue;
        }
        lines.push(raw.to_owned());
        if table == "daemon" && trimmed.starts_with('[') && !added {
            lines.push(line.clone());
            added = true;
        }
    }
    let mut new = lines.join("\n");
    if text.ends_with('\n') || !added {
        new.push('\n');
    }
    if !added {
        new.push_str(&format!("\n[daemon]\n{line}\n"));
    }
    match new.parse::<toml::Table>() {
        Ok(parsed) if removed && parsed == expected => Ok(Some(new)),
        _ => Err(()),
    }
}

/// `text` as [`rewrite`] writes it for [`REVIEWER`]: none when it has no
/// `[judge]`. Its heading becomes `[reviewer]`, its keys and comments as
/// written. What is written must parse to the config with only the table
/// renamed, so a `[reviewer]` the file has already, or a `judge` lines
/// can't rename (a dotted key, an inline table), is an error.
fn reviewer_named(text: &str) -> Result<Option<String>, ()> {
    let old: toml::Table = text.parse().map_err(drop)?;
    let Some(judge) = old.get("judge") else {
        return Ok(None);
    };
    if old.contains_key("reviewer") {
        return Err(());
    }
    let mut expected = old.clone();
    expected.remove("judge");
    expected.insert("reviewer".into(), judge.clone());
    let mut renamed = 0;
    let lines: Vec<String> = (text.split_inclusive('\n'))
        .map(|raw| {
            let indent = &raw[..raw.len() - raw.trim_start().len()];
            match raw.trim_start().strip_prefix("[judge]") {
                Some(rest) if rest.trim().is_empty() || rest.trim_start().starts_with('#') => {
                    renamed += 1;
                    format!("{indent}[reviewer]{rest}")
                }
                _ => raw.to_owned(),
            }
        })
        .collect();
    let new = lines.concat();
    match new.parse::<toml::Table>() {
        Ok(parsed) if renamed == 1 && parsed == expected => Ok(Some(new)),
        _ => Err(()),
    }
}

/// `text` as [`rewrite`] writes it for [`QUIET_GONE`]: none when `[sonny]`
/// sets neither `hours` nor `quiet_after`.
fn quiet_gone(text: &str) -> Result<Option<String>, ()> {
    sonny_keys_gone(text, &["hours", "quiet_after"])
}

/// `text` as [`rewrite`] writes it for [`CHECKS_GONE`]: none when `[sonny]`
/// sets neither `needs_check` nor `task_done_check`, which `/config` wrote.
fn checks_gone(text: &str) -> Result<Option<String>, ()> {
    sonny_keys_gone(text, &["needs_check", "task_done_check"])
}

/// `text` as [`rewrite`] writes it for [`VIEW_ONE`]: none when `[sonny]`
/// sets neither `steps` nor `verbose`. Their lines go, and the view they
/// showed is written under `[sonny]`'s heading: `verbose` on is every tool
/// call, `steps` on the steps, and neither the words, the default, so no
/// line. A `view` the file sets already stays as set.
fn view_one(text: &str) -> Result<Option<String>, ()> {
    let old: toml::Table = text.parse().map_err(drop)?;
    let Some(sonny) = old.get("sonny").and_then(toml::Value::as_table) else {
        return Ok(None);
    };
    // A switch set to anything but true or false is left for the person
    // to see: its meaning can't be carried over.
    if ["steps", "verbose"]
        .iter()
        .any(|key| sonny.get(*key).is_some_and(|value| !value.is_bool()))
    {
        return Err(());
    }
    let on = |key: &str| sonny.get(key).and_then(toml::Value::as_bool) == Some(true);
    let view = match (on("verbose"), on("steps")) {
        _ if sonny.contains_key("view") => None,
        (true, _) => Some("verbose"),
        (_, true) => Some("steps"),
        _ => None,
    };
    let Some(gone) = sonny_keys_gone(text, &["steps", "verbose"])? else {
        return Ok(None);
    };
    let Some(view) = view else {
        return Ok(Some(gone));
    };
    let mut expected: toml::Table = gone.parse().map_err(drop)?;
    if let Some(toml::Value::Table(sonny)) = expected.get_mut("sonny") {
        sonny.insert("view".into(), toml::Value::String(view.into()));
    }
    let mut new = String::new();
    let mut added = false;
    for raw in gone.split_inclusive('\n') {
        new.push_str(raw);
        let heading = raw.trim().strip_prefix("[sonny]");
        if !added
            && heading.is_some_and(|rest| rest.is_empty() || rest.trim_start().starts_with('#'))
        {
            if !raw.ends_with('\n') {
                new.push('\n');
            }
            new.push_str(&format!("view = \"{view}\"\n"));
            added = true;
        }
    }
    match new.parse::<toml::Table>() {
        Ok(parsed) if added && parsed == expected => Ok(Some(new)),
        _ => Err(()),
    }
}

/// `text` as [`rewrite`] writes it for [`TAB_WORDS`]: none when `[sonny]`
/// sets neither `task_windows` nor `side_chat = "fork"`. The key is
/// renamed and the value said again, on their own lines, each comment
/// kept. What is written must parse to the config with only those
/// changed, so a `most_task_tabs` set already, or a line the rewrite
/// does not find, is an error.
fn tab_words(text: &str) -> Result<Option<String>, ()> {
    use toml::Value;
    let old: toml::Table = text.parse().map_err(drop)?;
    let Some(sonny) = old.get("sonny").and_then(Value::as_table) else {
        return Ok(None);
    };
    let fork = sonny.get("side_chat").and_then(Value::as_str) == Some("fork");
    if !fork && !sonny.contains_key("task_windows") {
        return Ok(None);
    }
    let mut expected = old.clone();
    let Some(Value::Table(sonny)) = expected.get_mut("sonny") else {
        return Err(());
    };
    if let Some(most) = sonny.remove("task_windows")
        && sonny.insert("most_task_tabs".into(), most).is_some()
    {
        return Err(());
    }
    if fork {
        sonny.insert("side_chat".into(), Value::String("work".into()));
    }
    let mut table = String::new();
    let mut new = String::new();
    for raw in text.split_inclusive('\n') {
        let trimmed = raw.trim_start();
        let indent = &raw[..raw.len() - trimmed.len()];
        // A heading, `[sonny]`, a comment after it or not.
        if let Some(heading) = trimmed
            .strip_prefix('[')
            .filter(|rest| !rest.starts_with('['))
            && let Some((name, _)) = heading.split_once(']')
        {
            table = name.trim().to_owned();
        }
        let line = match (table == "sonny", trimmed.split_once('=')) {
            (true, Some((key, rest))) if key.trim() == "task_windows" => {
                format!("{indent}most_task_tabs ={rest}")
            }
            (true, Some((key, rest))) if key.trim() == "side_chat" => {
                let value = rest.trim_start();
                match value
                    .strip_prefix("\"fork\"")
                    .or(value.strip_prefix("'fork'"))
                {
                    Some(after) => format!("{indent}side_chat = \"work\"{after}"),
                    None => raw.to_owned(),
                }
            }
            _ => raw.to_owned(),
        };
        new.push_str(&line);
    }
    match new.parse::<toml::Table>() {
        Ok(parsed) if parsed == expected => Ok(Some(new)),
        _ => Err(()),
    }
}

/// `text` without the `[sonny]` keys `gone`: none when it sets none of
/// them. Their lines go, a list run over several lines whole, and every
/// other line stays as written. What is written must parse to the config
/// with only those keys gone, so a key lines can't take away (a dotted
/// key, an inline table) is an error.
fn sonny_keys_gone(text: &str, gone: &[&str]) -> Result<Option<String>, ()> {
    let old: toml::Table = text.parse().map_err(drop)?;
    let mut expected = old.clone();
    let Some(toml::Value::Table(sonny)) = expected.get_mut("sonny") else {
        return Ok(None);
    };
    if gone
        .iter()
        .filter(|key| sonny.remove(**key).is_some())
        .count()
        == 0
    {
        return Ok(None);
    }
    let (mut table, mut list) = (String::new(), false);
    let mut new = String::new();
    for raw in text.split_inclusive('\n') {
        let trimmed = raw.trim();
        if list {
            list = !trimmed.contains(']');
            continue;
        }
        // A heading, `[sonny]`, a comment after it or not.
        if let Some(heading) = trimmed
            .strip_prefix('[')
            .filter(|rest| !rest.starts_with('['))
            && let Some((name, _)) = heading.split_once(']')
        {
            table = name.trim().to_owned();
        }
        let set = trimmed.split_once('=');
        if let Some((key, value)) = set
            && table == "sonny"
            && gone.contains(&key.trim())
        {
            let value = value.trim();
            list = value.starts_with('[') && !value.contains(']');
            continue;
        }
        new.push_str(raw);
    }
    match new.parse::<toml::Table>() {
        Ok(parsed) if parsed == expected => Ok(Some(new)),
        _ => Err(()),
    }
}

/// The `sonny-config` skill: where the settings live and every key of the
/// file, from [`SETTINGS`], so it says what this Sonny reads.
pub fn skill() -> String {
    let rows: Vec<String> = SETTINGS
        .iter()
        .map(|setting| {
            let values = match setting.values {
                [] if number(setting) => "a whole number".to_owned(),
                [] if span(setting) => "a span, as `30s`, `15m` or `1h`".to_owned(),
                [] => "a name".to_owned(),
                values => (values.iter())
                    .map(|value| format!("`{value}`"))
                    .collect::<Vec<_>>()
                    .join(", "),
            };
            let listed = match setting.group {
                "" => "the file only".to_owned(),
                group => format!("{group}: {}", setting.name),
            };
            format!(
                "| `{}` | {} | {values} | {} | {listed} |",
                setting.key, setting.about, setting.default
            )
        })
        .collect();
    let table = format!(
        "| Key | What it sets | Values | Unset | In `/config` |\n| --- | --- | --- | --- | --- |\n{}",
        rows.join("\n")
    );
    include_str!("../skills/sonny-config/SKILL.md")
        .replace("{{VERSION}}", env!("CARGO_PKG_VERSION"))
        .replace("{{SETTINGS}}", &table)
}

/// What the keys say: open in `/config`.
pub enum Key {
    Stay,
    Close,
}

/// The settings `/config` lists, by their place in [`SETTINGS`], in its
/// groups' order.
fn listed() -> Vec<usize> {
    (GROUPS.iter())
        .flat_map(|group| (SETTINGS.iter().enumerate()).filter(move |(_, s)| s.group == *group))
        .map(|(at, _)| at)
        .collect()
}

/// `/config`: a list like any other, its settings under their groups'
/// names, typing narrowing it to the settings it finds.
pub struct View {
    path: PathBuf,
    /// Each setting's value in the file, `None` for its default.
    values: Vec<Option<String>>,
    /// The settings listed, by their place in [`SETTINGS`].
    listed: Vec<usize>,
    /// The listed settings' names and labels, narrowed and picked.
    picker: crate::list::Picker,
    /// The name being typed for the picked setting.
    typing: Option<String>,
    /// What the last change came to: saved, or why not.
    said: Option<Result<String, String>>,
}

impl View {
    /// The file as it is; an unreadable or invalid one is refused, as
    /// every other reader refuses it.
    pub fn open() -> Result<Self, String> {
        let path = agent_client::config::path().ok_or("HOME is not set")?;
        let mut view = Self::at(path);
        view.values = view.read()?;
        Ok(view)
    }

    /// The view of the file at `path`, not yet read.
    fn at(path: PathBuf) -> Self {
        let listed = listed();
        let rows = (listed.iter())
            .map(|&at| (SETTINGS[at].name.to_owned(), SETTINGS[at].label.to_owned()))
            .collect();
        View {
            path,
            values: vec![None; SETTINGS.len()],
            listed,
            picker: crate::list::Picker::new(rows, 0),
            typing: None,
            said: None,
        }
    }

    /// The setting picked, by its place in [`SETTINGS`]; none when typing
    /// finds none.
    fn picked(&self) -> Option<usize> {
        self.picker.chosen().map(|at| self.listed[at])
    }

    fn read(&self) -> Result<Vec<Option<String>>, String> {
        agent_client::config::read(&self.path)?;
        let text = std::fs::read_to_string(&self.path).unwrap_or_default();
        let table: toml::Table = text.parse().unwrap_or_default();
        Ok(SETTINGS
            .iter()
            .map(|setting| {
                // A key under a table, a table's within another's too.
                let mut path = setting.key.split('.');
                let first = table.get(path.next().unwrap_or_default());
                let value = path.try_fold(first, |at, key| Some(at?.get(key))).flatten();
                value.and_then(|v| match v {
                    toml::Value::Boolean(on) => Some(on.to_string()),
                    toml::Value::Integer(n) => Some(n.to_string()),
                    toml::Value::Array(list) if list.is_empty() => Some("none".into()),
                    toml::Value::String(text) if text.is_empty() => Some("none".into()),
                    toml::Value::Array(list) => Some(
                        (list.iter().filter_map(toml::Value::as_str))
                            .collect::<Vec<_>>()
                            .join(", "),
                    ),
                    v => v.as_str().map(str::to_owned),
                })
            })
            .collect())
    }

    /// Writes setting `at`'s `value`, `None` for its default, and reads
    /// the file again.
    fn write(&mut self, at: usize, value: Option<String>) {
        let setting = &SETTINGS[at];
        let value = value.map(|value| match (setting.values == SWITCH, number(setting)) {
            _ if setting.values == TRAILERS => toml::Value::Array(
                (value.split(", ").filter(|part| *part != "none"))
                    .map(|part| toml::Value::String(part.to_owned()))
                    .collect(),
            ),
            // No lookout trailer is written as an empty one; nothing
            // typed is the default again.
            _ if setting.key == "sonny.lookout_trailer" && value == "none" => {
                toml::Value::String(String::new())
            }
            (true, _) => toml::Value::Boolean(value == "true"),
            (_, true) => value
                .parse()
                .map_or(toml::Value::String(value), toml::Value::Integer),
            _ => toml::Value::String(value),
        });
        let written = agent_client::config::write(&self.path, &[(setting.key, value.clone())])
            .and_then(|()| self.read());
        self.said = Some(match written {
            Ok(values) => {
                self.values = values;
                let said = match &value {
                    Some(value) => format!("{} = {value}", setting.name),
                    None => format!("{} is its default again", setting.name),
                };
                // The setting is the job: it is set up or taken away now.
                match setting.key {
                    "sonny.start_at_boot" => {
                        let on = value.as_ref().and_then(toml::Value::as_bool) == Some(true);
                        crate::boot::apply(on).map(|done| format!("{said}: {done}"))
                    }
                    _ => Ok(said),
                }
            }
            Err(error) => Err(error),
        });
    }

    /// The next of the picked setting's values, `by` steps on, through
    /// its default and round again.
    fn step(&mut self, by: isize) {
        let Some(at) = self.picked() else {
            return;
        };
        let setting = &SETTINGS[at];
        if setting.values.is_empty() {
            return;
        }
        // The default first, then each value that is not the default
        // written out.
        let values: Vec<&str> = (setting.values.iter())
            .copied()
            .filter(|value| *value != setting.default)
            .collect();
        let now = match &self.values[at] {
            None => 0,
            Some(value) => (values.iter())
                .position(|v| v == value)
                .map_or(0, |at| at as isize + 1),
        };
        let next = (now + by).rem_euclid(values.len() as isize + 1) as usize;
        self.write(at, next.checked_sub(1).map(|at| values[at].to_owned()));
    }

    pub fn event(&mut self, event: &Event) -> Key {
        if let Event::Paste(text) = event
            && let Some(typed) = &mut self.typing
        {
            typed.push_str(text.lines().next().unwrap_or_default());
            return Key::Stay;
        }
        if let Event::Key(key) = event
            && key.kind != KeyEventKind::Release
            && let Some(typed) = &mut self.typing
        {
            match key.code {
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Key::Close;
                }
                KeyCode::Char(c) => typed.push(c),
                KeyCode::Backspace => {
                    typed.pop();
                }
                KeyCode::Esc => self.typing = None,
                KeyCode::Enter => {
                    let typed = self.typing.take().unwrap_or_default();
                    let typed = typed.trim();
                    if let Some(at) = self.picked() {
                        self.write(at, (!typed.is_empty()).then(|| typed.to_owned()));
                    }
                }
                _ => {}
            }
            return Key::Stay;
        }
        let Event::Key(key) = event else {
            if let Event::Paste(_) = event {
                self.said = None;
                self.picker.event(event);
            }
            return Key::Stay;
        };
        if key.kind == KeyEventKind::Release {
            return Key::Stay;
        }
        self.said = None;
        let picked = self.picked();
        let named = picked.is_some_and(|at| SETTINGS[at].values.is_empty());
        let set = picked.is_some_and(|at| self.values[at].is_some());
        match key.code {
            KeyCode::Left => self.step(-1),
            KeyCode::Right => self.step(1),
            KeyCode::Enter if named => {
                self.typing = picked.map(|at| self.values[at].clone().unwrap_or_default())
            }
            KeyCode::Enter => self.step(1),
            // With nothing typed to take back, the picked value goes back
            // to its default.
            KeyCode::Backspace if self.picker.filter().is_empty() && set => {
                if let Some(at) = picked {
                    self.write(at, None);
                }
            }
            KeyCode::Delete if set => {
                if let Some(at) = picked {
                    self.write(at, None);
                }
            }
            _ => {
                if self.picker.event(event) == crate::list::Act::Close {
                    return Key::Close;
                }
            }
        }
        Key::Stay
    }

    /// The listed settings `width` wide under a blank row, at most a
    /// list's rows and only `room` in all, the pick among them: under
    /// their groups' bold names with nothing typed, best first once typing
    /// narrows them. Each is its name, its value, and what it does, muted;
    /// a value set in the file is in ink and a default muted, so what was
    /// changed stands out.
    pub fn rows(&self, width: usize, room: usize) -> Vec<Line<'static>> {
        let shown = self.picker.shown();
        let pick = self.picker.at();
        let wide = (self.listed.iter())
            .map(|&at| SETTINGS[at].name.width())
            .max()
            .unwrap_or(0);
        let value = |at: usize| match &self.values[at] {
            Some(value) => value.clone(),
            None => SETTINGS[at].default.to_owned(),
        };
        // Values past this many columns are cut, so the labels line up.
        const VALUE_MAX: usize = 16;
        let values_wide = (shown.iter())
            .map(|(row, _)| value(self.listed[*row]).width().min(VALUE_MAX))
            .max()
            .unwrap_or(0);
        let mut lines = Vec::new();
        let mut picked = 0;
        let mut group = "";
        let grouped = self.picker.filter().is_empty();
        for (i, (row, found)) in shown.iter().enumerate() {
            let at = self.listed[*row];
            let setting = &SETTINGS[at];
            if grouped && setting.group != group {
                group = setting.group;
                lines.push(Line::from(format!("  {group}")).bold());
            }
            let typed = (i == pick).then_some(self.typing.as_ref()).flatten();
            let shown = match typed {
                Some(typed) => typed.clone(),
                None => crate::transcript::cut(&value(at), VALUE_MAX),
            };
            let pad = " ".repeat(wide.saturating_sub(setting.name.width()));
            let gap = " ".repeat(values_wide.saturating_sub(shown.width()) + 2);
            let room = width.saturating_sub(crate::list::INDENT + wide + 2 + values_wide + 2 + 1);
            let mut spans = vec![Span::raw(" ".repeat(crate::list::INDENT))];
            for (at, c) in setting.name.chars().enumerate() {
                let c = Span::raw(c.to_string());
                spans.push(if found.contains(&at) { c.bold() } else { c });
            }
            spans.push(Span::raw(format!("{pad}  ")));
            spans.push(match self.values[at].is_some() || typed.is_some() {
                true => Span::raw(shown),
                false => Span::raw(shown).dim(),
            });
            spans.push(Span::raw(gap));
            spans.push(Span::raw(crate::transcript::cut(setting.label, room)).dim());
            let mut line = Line::from(spans);
            if i == pick {
                picked = lines.len();
                crate::list::pick(&mut line);
            }
            lines.push(line);
        }
        let shown = lines
            .len()
            .min(crate::list::ROWS)
            .min(room.saturating_sub(1));
        let first = (picked + 1)
            .saturating_sub(shown)
            .min(lines.len().saturating_sub(shown));
        // The group's name over its first row, where there is room.
        let first = match first.checked_sub(1) {
            Some(above) if picked < first + shown - 1 && lines[above].spans.len() == 1 => above,
            _ => first,
        };
        let mut out = vec![Line::default()];
        out.extend(lines.into_iter().skip(first).take(shown));
        out
    }

    /// The composer's line: the name being typed, what the last change
    /// came to, or what is typed to find a setting; and where the cursor
    /// goes when one is.
    pub fn line(&self) -> (Line<'static>, Option<u16>) {
        if let Some(typed) = &self.typing {
            let line = Line::from(vec![Span::raw("› ").bold(), Span::raw(typed.clone())]);
            return (line, Some(2 + typed.width() as u16));
        }
        let what = match &self.said {
            Some(Ok(saved)) => Span::raw(format!("saved {saved}")).fg(Color::Green),
            Some(Err(error)) => Span::raw(format!("✗ {error}")),
            None => {
                let (line, cursor) = self.picker.line("find a setting");
                return (line, Some(cursor));
            }
        };
        (Line::from(vec![Span::raw("› ").bold(), what]), None)
    }

    /// The keys the picked setting takes, in `width` columns. The muted
    /// words after them come only where they fit whole.
    pub fn keys(&self, width: usize) -> Line<'static> {
        let file = crate::tilde(&self.path.display().to_string());
        use crate::hint::{Enter, Esc, Keys};
        let picked = self.picked();
        let named = picked.is_some_and(|at| SETTINGS[at].values.is_empty());
        let mut keys = match (&self.typing, named) {
            (Some(_), _) => Keys::new().enter(Enter::Save).esc(Esc::Cancel),
            (None, true) => Keys::new().pick().enter(Enter::Choose),
            (None, false) => Keys::new().pick().key("←→", "change"),
        };
        if self.typing.is_none() {
            if picked.is_some_and(|at| self.values[at].is_some()) {
                keys = keys.key("⌫", "default");
            }
            keys = keys.esc(Esc::Close);
        }
        let mut spans = crate::hint::under(&keys, width);
        let after = match self.typing {
            Some(_) => "  empty for the default".to_owned(),
            None => format!("  {file}"),
        };
        if Line::from(spans.clone()).width() + after.width() <= width {
            spans.push(Span::raw(after).dim());
        }
        Line::from(spans)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_config_skill_lists_every_key_as_this_sonny_reads_it() {
        let skill = skill();
        assert!(skill.starts_with("---\nname: sonny-config\n"));
        assert!(!skill.contains("{{"));
        assert!(skill.contains(env!("CARGO_PKG_VERSION")));
        for setting in SETTINGS {
            assert!(
                skill.contains(&format!("| `{}` |", setting.key)),
                "{}",
                setting.key
            );
        }
        assert!(skill.contains("| `sonny.margin_opens` |"));
        assert!(skill.contains("`always`, `unless-closed`, `never` | always |"));
        assert!(skill.contains("| `sonny.commons_invites` |"));
        assert!(skill.contains("| a whole number | 3 | Commons and sky: commons invites |"));
        assert!(skill.contains("| `sonny.tool_processes` |"));
        assert!(skill.contains("| 0 | the file only |"));
    }
    use ratatui::crossterm::event::KeyEvent;

    fn press(view: &mut View, code: KeyCode) {
        view.event(&Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    #[test]
    fn a_line_depth_set_for_sonny_moves_to_the_daemon() {
        // Into the daemon's table where there is one, the rest as written.
        let text = "# mine\n[sonny]\nchime = true\ntask_depth = 3\n\n[daemon]\nload = 0\n";
        assert_eq!(
            depth_moved(text),
            Ok(Some(
                "# mine\n[sonny]\nchime = true\n\n[daemon]\ndepth = 3\nload = 0\n".into()
            ))
        );
        // Into one of its own where there is none.
        assert_eq!(
            depth_moved("[sonny]\ntask_depth = 0\n"),
            Ok(Some("[sonny]\n\n[daemon]\ndepth = 0\n".into()))
        );
        // One the daemon's table sets already stays as set.
        assert_eq!(
            depth_moved("[daemon]\ndepth = 4\n[sonny]\ntask_depth = 3\n"),
            Ok(Some("[daemon]\ndepth = 4\n[sonny]\n".into()))
        );
        // A heading with a comment after it is its table all the same.
        assert_eq!(
            depth_moved("[sonny] # mine\ntask_depth = 3\n[daemon] # its\nload = 0\n"),
            Ok(Some(
                "[sonny] # mine\n[daemon] # its\ndepth = 3\nload = 0\n".into()
            ))
        );
        // Nothing to move.
        assert_eq!(depth_moved("[sonny]\nchime = true\n"), Ok(None));
        // A key lines can't move is left for the reader's error.
        assert_eq!(depth_moved("sonny.task_depth = 3\n"), Err(()));
    }

    #[test]
    fn the_judge_table_is_renamed_the_reviewer_s() {
        // Its heading only: keys, comments and the rest as written.
        let text = "# mine\nmodel = \"a/b\"\n\n[judge] # cheap\nmodel = \"c/d\"\neffort = \"low\"\n\n[sonny]\nchime = true\n";
        assert_eq!(
            reviewer_named(text),
            Ok(Some(
                "# mine\nmodel = \"a/b\"\n\n[reviewer] # cheap\nmodel = \"c/d\"\neffort = \"low\"\n\n[sonny]\nchime = true\n".into()
            ))
        );
        // Nothing to rename.
        assert_eq!(reviewer_named("[sonny]\nchime = true\n"), Ok(None));
        // Lines can't rename these: left for the reader's error.
        assert_eq!(reviewer_named("judge.model = \"c/d\"\n"), Err(()));
        assert_eq!(reviewer_named("judge = { model = \"c/d\" }\n"), Err(()));
        assert_eq!(
            reviewer_named("[judge]\nmodel = \"c/d\"\n[reviewer]\neffort = \"low\"\n"),
            Err(())
        );
    }

    #[test]
    fn quiet_mode_s_keys_go_and_the_rest_stays_as_written() {
        let text = "[sonny]\nchime = true\nhours = [\"Mon-Fri 09:00-19:00\"]  # mine\nquiet_after = \"45m\"\nverbose = true\n\n[daemon]\ndepth = 2\n";
        assert_eq!(
            quiet_gone(text),
            Ok(Some(
                "[sonny]\nchime = true\nverbose = true\n\n[daemon]\ndepth = 2\n".into()
            ))
        );
        // A list over several lines goes whole.
        let text = "[sonny]\nhours = [\n  \"Mon-Fri 09:00-19:00\",\n  \"Sat 10:00-12:00\",\n]\nchime = true\n";
        assert_eq!(quiet_gone(text), Ok(Some("[sonny]\nchime = true\n".into())));
        // A heading with a comment after it is the table all the same.
        assert_eq!(
            quiet_gone("[sonny] # mine\nhours = [\"Mon 09:00-17:00\"]\nchime = true\n"),
            Ok(Some("[sonny] # mine\nchime = true\n".into()))
        );
        // Nothing to take away, here or in another table.
        assert_eq!(quiet_gone("[sonny]\nchime = true\n"), Ok(None));
        assert_eq!(quiet_gone("[daemon]\ndepth = 2\n"), Ok(None));
        // A key lines can't take away is left for the reader's error.
        assert_eq!(quiet_gone("sonny.quiet_after = \"30m\"\n"), Err(()));
    }

    #[test]
    fn the_removed_checks_keys_go_and_the_rest_stays_as_written() {
        let text = "[sonny]\nchime = true\nneeds_check = false\ntask_done_check = 24  # mine\nverbose = true\n";
        assert_eq!(
            checks_gone(text),
            Ok(Some("[sonny]\nchime = true\nverbose = true\n".into()))
        );
        assert_eq!(checks_gone("[sonny]\nchime = true\n"), Ok(None));
        assert_eq!(checks_gone("sonny.needs_check = false\n"), Err(()));
    }

    /// The longest a setting's label runs, as the Rulebook's `label-max`.
    const LABEL_MAX: usize = 70;

    /// Narrows `view` to the setting named `name`, as typing it does.
    fn find(view: &mut View, name: &str) {
        view.picker.narrow(name.to_owned());
        assert_eq!(view.picked().map(|at| SETTINGS[at].name), Some(name));
    }

    #[test]
    fn every_key_the_file_takes_is_listed() {
        // A key or value the reader does not take would be refused.
        let dir = std::env::temp_dir().join(format!("sonny-config-keys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut view = View::at(dir.join("config.toml"));
        for (at, setting) in SETTINGS.iter().enumerate() {
            let named = match (number(setting), span(setting)) {
                // Wide enough for a text width.
                (true, _) => "70",
                (_, true) => "15m",
                _ => "a/b",
            };
            let value = setting.values.last().copied().unwrap_or(named);
            view.write(at, Some(value.to_owned()));
            assert!(matches!(view.said, Some(Ok(_))), "{:?}", view.said);
            assert_eq!(view.values[at].as_deref(), Some(value));
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn config_lists_the_groups_with_one_line_labels_and_keeps_tuning_in_the_file() {
        // Every group has settings, and every setting a person changes is
        // in one, under a plain name and a label of one short line.
        for group in GROUPS {
            assert!(SETTINGS.iter().any(|s| s.group == *group), "{group}");
        }
        for setting in SETTINGS {
            assert!(
                setting.group.is_empty() || GROUPS.contains(&setting.group),
                "{}",
                setting.key
            );
            if !setting.group.is_empty() {
                assert!(
                    !setting.label.is_empty()
                        && setting.label.chars().count() <= LABEL_MAX
                        && !setting.label.contains('\n'),
                    "{}: {}",
                    setting.key,
                    setting.label
                );
                assert!(!setting.name.contains(['_', '.']), "{}", setting.name);
            }
        }
        assert_eq!(listed().len(), 39);
        // What only tunes stays in the file, and in the reference.
        for key in [
            "sonny.sky_haiku_daily",
            "sonny.sky_haiku_keep",
            "sonny.sky_haiku_window",
            "sonny.sky_haiku_margin",
            "sonny.sky_haiku_hold",
            "sonny.commons_invite_tokens",
            "sonny.hold_quiet",
            "sonny.tool_processes",
            "sonny.lookout_waiting",
        ] {
            let at = SETTINGS.iter().position(|s| s.key == key).unwrap();
            assert!(!listed().contains(&at), "{key}");
            assert!(skill().contains(&format!("| `{key}` |")), "{key}");
        }
    }

    #[test]
    fn config_is_a_list_of_eight_rows_under_bold_group_names_that_typing_narrows() {
        let mut view = View::at(PathBuf::from("/nowhere/config.toml"));
        view.values[0] = Some("openai/gpt".into());
        let text =
            |lines: &[Line]| -> Vec<String> { lines.iter().map(|l| l.to_string()).collect() };
        let rows = view.rows(100, 40);
        assert_eq!(rows.len(), 1 + crate::list::ROWS, "{:?}", text(&rows));
        assert_eq!(rows[0].to_string(), "");
        assert_eq!(rows[1].to_string(), "  Models");
        assert!(
            rows[1]
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
        // The pick reversed; a value set in ink, a default muted.
        let model = rows[2].to_string();
        assert!(
            model.starts_with("    model  ") && model.contains("openai/gpt"),
            "{model}"
        );
        assert!(rows[2].spans.iter().skip(1).all(|s| {
            s.style
                .add_modifier
                .contains(ratatui::style::Modifier::REVERSED)
        }));
        let provider = &rows[3];
        let default = provider
            .spans
            .iter()
            .find(|s| s.content == "AGENT_PROVIDER")
            .unwrap();
        assert!(
            default
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::DIM)
        );
        assert!(
            rows.iter().all(|row| row.width() <= 100),
            "{:?}",
            text(&rows)
        );
        // Down past the eighth row scrolls, the pick in view.
        for _ in 0..9 {
            press(&mut view, KeyCode::Down);
        }
        let rows = view.rows(100, 40);
        assert_eq!(rows.len(), 1 + crate::list::ROWS);
        assert!(
            text(&rows)
                .iter()
                .any(|row| row.trim_start().starts_with("lead profile")),
            "{:?}",
            text(&rows)
        );
        // Up from the first wraps to the last.
        let mut view = View::at(PathBuf::from("/nowhere/config.toml"));
        press(&mut view, KeyCode::Up);
        assert_eq!(
            view.picked().map(|at| SETTINGS[at].name),
            Some("start at boot")
        );
        // Typing narrows, best first, without group names.
        let mut view = View::at(PathBuf::from("/nowhere/config.toml"));
        for c in "view".chars() {
            press(&mut view, KeyCode::Char(c));
        }
        assert_eq!(view.picked().map(|at| SETTINGS[at].key), Some("sonny.view"));
        let rows = text(&view.rows(100, 40));
        assert!(rows[1].starts_with("    view "), "{rows:?}");
        assert!(
            rows.iter().all(|row| !row.starts_with("  View")),
            "{rows:?}"
        );
        assert_eq!(view.line().0.to_string(), "› view");
        // Backspace takes a letter back; Esc closes.
        press(&mut view, KeyCode::Backspace);
        assert_eq!(view.picker.filter(), "vie");
        assert!(matches!(
            view.event(&Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))),
            Key::Close
        ));
        // Nothing typed, the composer asks.
        let view = View::at(PathBuf::from("/nowhere/config.toml"));
        assert_eq!(view.line().0.to_string(), "› find a setting");
        assert_eq!(
            view.keys(80).to_string(),
            "  ↑↓ pick · enter choose · esc close  /nowhere/config.toml"
        );
        // Short of room, the file goes whole, then the keys as every
        // key line's do: no word is cut.
        assert_eq!(
            view.keys(50).to_string(),
            "  ↑↓ pick · enter choose · esc close"
        );
        assert_eq!(view.keys(30).to_string(), "  enter choose · esc close");
    }

    #[test]
    fn steps_and_verbose_become_one_view_once() {
        // Verbose on is every tool call, whatever steps says.
        assert_eq!(
            view_one("# mine\n[sonny] # me\nchime = true\nsteps = true\nverbose = true\n"),
            Ok(Some(
                "# mine\n[sonny] # me\nview = \"verbose\"\nchime = true\n".into()
            ))
        );
        assert_eq!(
            view_one("[sonny]\nsteps = true\nverbose = false\n[daemon]\ndepth = 2\n"),
            Ok(Some(
                "[sonny]\nview = \"steps\"\n[daemon]\ndepth = 2\n".into()
            ))
        );
        // Neither on is the words, the default: no line.
        assert_eq!(
            view_one("[sonny]\nsteps = false\nchime = true\n"),
            Ok(Some("[sonny]\nchime = true\n".into()))
        );
        // A view set already stays as set.
        assert_eq!(
            view_one("[sonny]\nview = \"words\"\nverbose = true\n"),
            Ok(Some("[sonny]\nview = \"words\"\n".into()))
        );
        // Nothing to rewrite, and lines that can't be rewritten.
        assert_eq!(view_one("[sonny]\nchime = true\n"), Ok(None));
        assert_eq!(view_one("[daemon]\ndepth = 2\n"), Ok(None));
        assert_eq!(view_one("sonny.verbose = true\n"), Err(()));
        // A switch that is not true or false is left as written.
        assert_eq!(view_one("[sonny]\nsteps = \"true\"\n"), Err(()));
        assert_eq!(view_one("[sonny]\nsteps = true\nverbose = 1\n"), Err(()));
        // What it writes, the reader takes.
        let dir =
            std::env::temp_dir().join(format!("sonny-config-view-one-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "[sonny]\nsteps = true\n").unwrap();
        assert!(agent_client::config::read(&path).is_err());
        assert!(rewrite(&path, view_one));
        assert_eq!(
            agent_client::config::read(&path).unwrap().sonny.view,
            agent_client::config::View::Steps
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn task_windows_and_a_fork_side_chat_take_the_rulebooks_words_once() {
        assert_eq!(
            tab_words(
                "# mine\n[sonny] # me\n  task_windows = 8 # few\nside_chat = \"fork\" # all\n"
            ),
            Ok(Some(
                "# mine\n[sonny] # me\n  most_task_tabs = 8 # few\nside_chat = \"work\" # all\n"
                    .into()
            ))
        );
        // Another table's keys of the same names are not Sonny's.
        assert_eq!(
            tab_words("[sonny]\nside_chat = \"answer\"\n[daemon]\ndepth = 2\n"),
            Ok(None)
        );
        // A key set under both names, or one lines can't rename, stays.
        assert_eq!(
            tab_words("[sonny]\ntask_windows = 8\nmost_task_tabs = 4\n"),
            Err(())
        );
        assert_eq!(tab_words("sonny.task_windows = 8\n"), Err(()));
        // What it writes, the reader takes.
        let dir =
            std::env::temp_dir().join(format!("sonny-config-tab-words-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "[sonny]\ntask_windows = 8\nside_chat = 'fork'\n").unwrap();
        assert!(agent_client::config::read(&path).is_err());
        assert!(rewrite(&path, tab_words));
        let sonny = agent_client::config::read(&path).unwrap().sonny;
        assert_eq!(sonny.most_task_tabs, 8);
        assert_eq!(sonny.side_chat, agent_client::config::SideChat::Work);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_value_the_reader_refuses_is_said_and_not_written() {
        let dir = std::env::temp_dir().join(format!("sonny-config-refused-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "[sonny]\ntext_width = 120\n").unwrap();
        let mut view = View::at(path.clone());
        view.values = view.read().unwrap();
        find(&mut view, "text width");
        press(&mut view, KeyCode::Enter);
        for _ in 0..3 {
            press(&mut view, KeyCode::Backspace);
        }
        press(&mut view, KeyCode::Char('2'));
        press(&mut view, KeyCode::Char('0'));
        press(&mut view, KeyCode::Enter);
        assert!(
            matches!(&view.said, Some(Err(e)) if e.contains("at least 40, not 20")),
            "{:?}",
            view.said
        );
        let at = view.picked().unwrap();
        assert_eq!(view.values[at].as_deref(), Some("120"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[sonny]\ntext_width = 120\n"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_lookout_trailer_typed_none_is_written_empty_and_nothing_typed_is_the_default() {
        let dir = std::env::temp_dir().join(format!("sonny-config-trailer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "[sonny]\n").unwrap();
        let mut view = View::at(path.clone());
        view.values = view.read().unwrap();
        find(&mut view, "lookout trailer");
        press(&mut view, KeyCode::Enter);
        for c in "none".chars() {
            press(&mut view, KeyCode::Char(c));
        }
        press(&mut view, KeyCode::Enter);
        let at = view.picked().unwrap();
        assert_eq!(view.values[at].as_deref(), Some("none"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[sonny]\nlookout_trailer = \"\"\n"
        );
        let trailer = |path| {
            agent_client::config::read(path)
                .unwrap()
                .sonny
                .lookout_trailer
        };
        assert_eq!(trailer(&path), "");
        press(&mut view, KeyCode::Enter);
        for _ in 0.."none".len() {
            press(&mut view, KeyCode::Backspace);
        }
        press(&mut view, KeyCode::Enter);
        assert_eq!(view.values[at], None);
        assert_eq!(trailer(&path), agent_client::config::LOOKOUT_TRAILER);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn values_step_through_the_default_and_names_are_typed() {
        let dir = std::env::temp_dir().join(format!("sonny-config-view-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "# mine\n[sonny]\nlead_effort = \"low\"\n").unwrap();
        let mut view = View::at(path.clone());
        view.values = view.read().unwrap();
        let value = |view: &View| view.values[view.picked().unwrap()].clone();
        find(&mut view, "lead effort");
        assert_eq!(value(&view).as_deref(), Some("low"));
        // The side chat: one that answers, then its default again either way.
        find(&mut view, "side chat");
        press(&mut view, KeyCode::Right);
        assert_eq!(value(&view).as_deref(), Some("answer"));
        assert!(matches!(&view.said, Some(Ok(said)) if said == "side chat = \"answer\""));
        press(&mut view, KeyCode::Right);
        assert_eq!(value(&view), None);
        press(&mut view, KeyCode::Left);
        assert_eq!(value(&view).as_deref(), Some("answer"));
        // A task's speed steps through the tiers, taken back to its starter's
        // with Delete, as Backspace takes back a letter typed.
        find(&mut view, "task speed");
        press(&mut view, KeyCode::Right);
        assert_eq!(value(&view).as_deref(), Some("standard"));
        press(&mut view, KeyCode::Delete);
        assert_eq!(value(&view), None);
        // Efforts step through every level.
        find(&mut view, "reviewer effort");
        press(&mut view, KeyCode::Left);
        assert_eq!(value(&view).as_deref(), Some("max"));
        press(&mut view, KeyCode::Enter);
        assert_eq!(value(&view), None);
        // The view steps through its three.
        find(&mut view, "view");
        press(&mut view, KeyCode::Right);
        assert_eq!(value(&view).as_deref(), Some("steps"));
        press(&mut view, KeyCode::Right);
        assert_eq!(value(&view).as_deref(), Some("verbose"));
        assert_eq!(
            agent_client::config::read(&path).unwrap().sonny.view,
            agent_client::config::View::Verbose
        );
        press(&mut view, KeyCode::Right);
        assert_eq!(value(&view), None);
        // A model is typed whole after Enter, and an empty one is the default.
        let mut view = View::at(path.clone());
        view.values = view.read().unwrap();
        assert_eq!(view.picked().map(|at| SETTINGS[at].key), Some("model"));
        press(&mut view, KeyCode::Enter);
        for c in "a/b".chars() {
            press(&mut view, KeyCode::Char(c));
        }
        assert_eq!(view.values[0], None);
        assert_eq!(view.line().0.to_string(), "› a/b");
        press(&mut view, KeyCode::Enter);
        assert_eq!(view.values[0].as_deref(), Some("a/b"));
        press(&mut view, KeyCode::Enter);
        for _ in 0..3 {
            press(&mut view, KeyCode::Backspace);
        }
        press(&mut view, KeyCode::Enter);
        assert_eq!(view.values[0], None);
        // The lead's effort back to its default with Backspace, nothing typed.
        press(&mut view, KeyCode::Down);
        press(&mut view, KeyCode::Down);
        assert_eq!(
            view.picked().map(|at| SETTINGS[at].name),
            Some("lead effort")
        );
        press(&mut view, KeyCode::Backspace);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# mine\n[sonny]\nside_chat = \"answer\"\n"
        );
        // A switch is written as TOML's own.
        find(&mut view, "chime");
        press(&mut view, KeyCode::Right);
        assert_eq!(value(&view).as_deref(), Some("true"));
        assert!(agent_client::config::read(&path).unwrap().sonny.chime);
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .ends_with("chime = true\n")
        );
        press(&mut view, KeyCode::Right);
        assert_eq!(value(&view), None);
        // A value the reader refuses is said, and the file kept.
        find(&mut view, "task effort");
        std::fs::write(&path, "[sonny]\nbogus = \"x\"\n").unwrap();
        press(&mut view, KeyCode::Right);
        assert!(matches!(&view.said, Some(Err(e)) if e.contains("unknown key sonny.bogus")));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[sonny]\nbogus = \"x\"\n"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
