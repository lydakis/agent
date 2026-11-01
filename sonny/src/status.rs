//! `/status`: the project's agents on one card, read from the daemon and
//! Rex with no model call. The lead comes first; its tasks follow grouped
//! by where they stand, as a thread list is: what needs the person, what
//! works, what waits for review, what sits idle, and, folded away, what the
//! lead marked resolved. Each row says what the agent is doing, for how
//! long, on which model and profile, its latest step, where it is in Rex, and what
//! runs for it in the background.
use ratatui::{
    style::{Color, Stylize},
    text::{Line, Span},
};

#[derive(Clone, Default)]
pub struct Row {
    /// The agent's name in its project: `lead`, or the task's.
    pub name: String,
    /// The agent's own name, which a click on its row opens.
    pub bot: String,
    pub state: String,
    /// Whether a turn of its runs.
    pub running: bool,
    /// Seconds its running turn has been at it, or its last turn took.
    pub seconds: Option<u64>,
    /// When its running turn started or its last turn ended (ms), which
    /// orders the rows.
    pub at: Option<u64>,
    pub model: String,
    /// The profile it runs as, its seat's own too: `lead`, `superlead`.
    pub profile: String,
    pub latest: String,
    /// Where it is in Rex: `tab 2`, `tab 1, split`, or empty.
    pub place: String,
    /// The pull request its last reply links, as `owner/repo#N`.
    pub pr: String,
    /// What runs for it with no one typing, as `/background` counts it:
    /// `↻ 2 triggers`, or empty.
    pub background: String,
    pub group: Group,
}

/// Where a task stands, as `/status` groups the tasks, in its order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
    /// Waiting on the person's approval or answer, or failed.
    NeedsYou,
    /// A turn running, queued, rate limited, or waiting on other tasks.
    Working,
    /// Finished with a pull request for someone to review.
    Review,
    /// Finished, stopped, or never started, with nothing open.
    #[default]
    Done,
    /// Marked resolved: nothing left to do, until new work reopens it.
    Resolved,
}

impl Group {
    fn heading(self) -> &'static str {
        match self {
            Group::NeedsYou => "Needs you",
            Group::Working => "Working",
            Group::Review => "Ready for review",
            Group::Done => "Done",
            Group::Resolved => "Resolved",
        }
    }
}

/// Where a task stands: `state` as `/status` names it, whether a turn
/// runs, whether it has a question open for the person or a pull request
/// in its last reply, and whether it is marked resolved, which only a
/// task with no turn running can be.
pub fn group(state: &str, running: bool, asking: bool, pr: bool, resolved: bool) -> Group {
    match state {
        _ if resolved && !running => Group::Resolved,
        "needs you" | "failed" => Group::NeedsYou,
        _ if asking => Group::NeedsYou,
        _ if running => Group::Working,
        "done" | crate::started::CHECKED if pr => Group::Review,
        _ => Group::Done,
    }
}

/// The pull request `text` links last, as `owner/repo#N`: a GitHub pull
/// or a GitLab merge request.
pub fn pull_request(text: &str) -> Option<String> {
    text.split(|c: char| c.is_whitespace() || "()<>[]\"'`".contains(c))
        .filter_map(|word| {
            let rest = word
                .strip_prefix("https://")
                .or_else(|| word.strip_prefix("http://"))?;
            let parts: Vec<&str> = rest.split('/').collect();
            let (repo, number) = match parts.as_slice() {
                [_, owner, repo, "pull", number, ..] => (format!("{owner}/{repo}"), *number),
                [_, path @ .., "-", "merge_requests", number] => (path.join("/"), *number),
                [_, path @ .., "-", "merge_requests", number, _] => (path.join("/"), *number),
                _ => return None,
            };
            let digits: String = number.chars().take_while(char::is_ascii_digit).collect();
            (!digits.is_empty() && !repo.is_empty()).then(|| format!("{repo}#{digits}"))
        })
        .next_back()
}

/// The mark and colour a state reads with.
pub fn mark(state: &str) -> Span<'static> {
    match state {
        "working" | crate::started::CHECKING => Span::raw("● ").fg(Color::Cyan),
        "needs you" => Span::raw("? ").fg(Color::Yellow).bold(),
        "done" | "completed" => Span::raw("✓ ").fg(Color::Green),
        crate::started::CHECKED => Span::raw("✓ ").fg(Color::Green),
        crate::started::QUIET => Span::raw("✓ ").dim(),
        "failed" => Span::raw("✗ ").fg(Color::Red),
        s if s.starts_with("waiting") || s == "queued" || s == "rate limited" => {
            Span::raw("◌ ").dim()
        }
        _ => Span::raw("○ ").dim(),
    }
}

/// A reply's first line of prose: fenced blocks and headings skipped,
/// list markers and emphasis left out.
pub fn plain_line(text: &str) -> String {
    let mut fence = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            fence = !fence;
            continue;
        }
        if fence || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let trimmed = ["- ", "* ", "+ ", "> "]
            .iter()
            .find_map(|m| trimmed.strip_prefix(m))
            .unwrap_or(trimmed);
        return trimmed.replace("**", "").replace('`', "");
    }
    String::new()
}

/// The lead, then its tasks by group, resolved ones only counted unless
/// `resolved` shows them; and the line each agent's rows start on, with
/// its bot, for a click there to open it.
pub fn card(
    project: &str,
    rows: &[Row],
    width: usize,
    resolved: bool,
) -> (Vec<Line<'static>>, Vec<(usize, String)>) {
    let mut named = Vec::new();
    let tasks: Vec<&Row> = rows.iter().filter(|r| r.name != "lead").collect();
    let count = |group: Group| tasks.iter().filter(|r| r.group == group).count();
    let mut head = format!(
        "{project} · {} task{}",
        tasks.len(),
        if tasks.len() == 1 { "" } else { "s" }
    );
    for (group, said) in [
        (Group::NeedsYou, "need you"),
        (Group::Working, "working"),
        (Group::Review, "to review"),
    ] {
        if count(group) > 0 {
            head.push_str(&format!(" · {} {said}", count(group)));
        }
    }
    let mut out = vec![Line::from(head).bold()];
    for row in rows.iter().filter(|r| r.name == "lead") {
        named.push((out.len(), row.bot.clone()));
        out.extend(row_lines(row, width));
    }
    for group in [
        Group::NeedsYou,
        Group::Working,
        Group::Review,
        Group::Done,
        Group::Resolved,
    ] {
        let n = count(group);
        if n == 0 {
            continue;
        }
        out.push(Line::default());
        if group == Group::Resolved && !resolved {
            out.push(Line::from(format!("{n} resolved · r shows them")).dim());
            continue;
        }
        out.push(Line::from(vec![
            Span::raw(group.heading()).bold(),
            Span::raw(format!(" · {n}")).dim(),
        ]));
        for row in tasks.iter().filter(|r| r.group == group) {
            named.push((out.len(), row.bot.clone()));
            out.extend(row_lines(row, width));
        }
    }
    (out, named)
}

/// Something wrong around the agents, which `/status` says under its
/// heading: a companion that keeps restarting, a skill to fix.
#[derive(Clone, Debug, PartialEq)]
pub struct Alert {
    /// Whether it is broken now (`✗`), not only something to tidy.
    pub failed: bool,
    pub said: String,
    /// Where to look, muted under it.
    pub detail: String,
}

/// How long a companion's run must last to count as healthy, as the
/// daemon counts it before its restart backoff starts over.
const HEALTHY_MS: u64 = 60_000;

/// The companions the daemon keeps (`companions`' list) that keep
/// restarting, as of `now_ms`: one that restarted and is down now,
/// waiting for its next start, or up for less than a healthy run. Each log
/// is in `logs`, the daemon's `STORE.companions`.
pub fn companion_alerts(companions: &serde_json::Value, now_ms: u64, logs: &str) -> Vec<Alert> {
    let listed = companions.as_array().into_iter().flatten();
    listed
        .filter(|c| {
            let restarts = c["restarts"].as_u64().unwrap_or(0);
            let up_for = c["started_ms"].as_u64().map(|at| now_ms.saturating_sub(at));
            restarts > 0 && (c["running"] != true || up_for.is_none_or(|up| up < HEALTHY_MS))
        })
        .map(|c| {
            let name = c["name"].as_str().unwrap_or("a companion");
            let restarts = c["restarts"].as_u64().unwrap_or(0);
            let exit = &c["last_exit"];
            let how = match (
                exit["error"].as_str(),
                exit["code"].as_i64(),
                exit["signal"].as_i64(),
            ) {
                (Some("spawn_failed"), ..) => "could not start".to_owned(),
                (Some(_), ..) => "lost track of it".to_owned(),
                (None, Some(code), _) => format!("exits {code}"),
                (None, None, Some(signal)) => format!("killed by signal {signal}"),
                (None, None, None) => "exits".to_owned(),
            };
            Alert {
                failed: true,
                said: format!(
                    "{name} keeps restarting · {how} · {restarts} restart{}",
                    if restarts == 1 { "" } else { "s" }
                ),
                detail: format!("its log: {logs}/{name}.log"),
            }
        })
        .collect()
}

/// The skills agents are offered whose front matter needs a fix, in one
/// alert: each by name and what is wrong.
pub fn skill_alerts(skills: &[agent_client::policy::Skill]) -> Vec<Alert> {
    let warned: Vec<String> = (skills.iter())
        .filter_map(|skill| Some(format!("{}: {}", skill.name, skill.warning?)))
        .collect();
    if warned.is_empty() {
        return Vec::new();
    }
    let n = warned.len();
    vec![Alert {
        failed: false,
        said: format!(
            "{n} skill{} to fix, agents may not know when to read {}",
            if n == 1 { "" } else { "s" },
            if n == 1 { "it" } else { "them" }
        ),
        detail: warned.join(" · "),
    }]
}

/// An alert's lines at `width`: a failure after its red `✗`, the rest
/// muted, and where to look muted under it.
pub fn alert_lines(alert: &Alert, width: usize) -> Vec<Line<'static>> {
    let head = match alert.failed {
        true => Line::from(vec![
            Span::raw("✗ ").fg(Color::Red),
            Span::raw(alert.said.clone()),
        ]),
        false => Line::from(alert.said.clone()).dim(),
    };
    let lead = if alert.failed { "  " } else { "" };
    let mut out = crate::transcript::wrap(head, width, Span::raw(""), Span::raw(lead));
    if !alert.detail.is_empty() {
        out.extend(crate::transcript::wrap(
            Line::from(alert.detail.clone()).dim(),
            width,
            Span::raw(lead),
            Span::raw(lead),
        ));
    }
    out
}

/// One agent's lines: its mark, name, state and time; its model, profile,
/// place and pull request; its latest step.
fn row_lines(row: &Row, width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    // How long the running turn has been at it, or the last one took.
    let time = match row.seconds {
        Some(s) if row.running => crate::transcript::elapsed(s),
        Some(s) => format!("took {}", crate::transcript::elapsed(s)),
        None => String::new(),
    };
    let mark = match row.group {
        Group::Resolved => Span::raw("✓ ").dim(),
        Group::Review => Span::raw("◆ ").fg(Color::Blue),
        _ => mark(&row.state),
    };
    let mut spans = vec![
        Span::raw(" "),
        mark,
        // In the agents' color: a click opens it.
        Span::raw(row.name.clone()).bold().fg(Color::Blue),
        Span::raw(format!(" {}", row.state)),
    ];
    if !time.is_empty() {
        spans.push(Span::raw(format!(" · {time}")).dim());
    }
    out.extend(crate::transcript::wrap(
        Line::from(spans),
        width,
        Span::raw(""),
        Span::raw("   "),
    ));
    let model = match row.model.as_str() {
        "" => String::new(),
        id => crate::models::named(id),
    };
    let profile = match row.profile.as_str() {
        "" => String::new(),
        name => format!("{name} profile"),
    };
    let facts: Vec<&str> = [
        model.as_str(),
        profile.as_str(),
        row.place.as_str(),
        row.pr.as_str(),
        row.background.as_str(),
    ]
    .into_iter()
    .filter(|f| !f.is_empty())
    .collect();
    if !facts.is_empty() {
        let line = Line::from(facts.join(" · ")).dim();
        out.extend(crate::transcript::wrap(
            line,
            width,
            Span::raw("   "),
            Span::raw("   "),
        ));
    }
    if !row.latest.is_empty() {
        // The agent's words, read as the Markdown they are.
        let line = Line::from(crate::transcript::inline(&row.latest, Default::default())).dim();
        out.extend(crate::transcript::capped(
            line,
            width,
            Span::raw("   └ ").dim(),
            1,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_latest_step_reads_as_prose() {
        assert_eq!(
            plain_line("```mermaid\nflowchart LR\n```\n## Done\n- Added `x` on **line 2**"),
            "Added x on line 2"
        );
    }

    #[test]
    fn a_companion_that_keeps_restarting_is_named_one_that_recovered_is_not() {
        let now = 10_000_000;
        let listed = serde_json::json!([
            // Never started again: fine.
            {"name": "sonny-lookout", "running": true, "started_ms": 1, "restarts": 0},
            // Restarted once, up for an hour since: recovered.
            {"name": "sonny-tool-host", "running": true, "started_ms": now - 3_600_000,
                "restarts": 1, "last_exit": {"code": 1}},
            // A removed binary: down, waiting for its next start.
            {"name": "approver-auto", "running": false, "started_ms": null, "restarts": 14,
                "last_exit": {"error": "spawn_failed", "detail": "No such file"}},
            // Up again only seconds ago, after dying on a signal.
            {"name": "helper", "running": true, "started_ms": now - 5_000, "restarts": 1,
                "last_exit": {"code": null, "signal": 9}},
        ]);
        let alerts = companion_alerts(&listed, now, "~/.agent/state.sqlite.companions");
        let said: Vec<&str> = alerts.iter().map(|a| a.said.as_str()).collect();
        assert_eq!(
            said,
            [
                "approver-auto keeps restarting · could not start · 14 restarts",
                "helper keeps restarting · killed by signal 9 · 1 restart",
            ]
        );
        assert!(alerts.iter().all(|a| a.failed));
        assert_eq!(
            alerts[0].detail,
            "its log: ~/.agent/state.sqlite.companions/approver-auto.log"
        );
        let lines = alert_lines(&alerts[0], 80);
        let mark = (lines[0].spans.iter())
            .find(|s| !s.content.is_empty())
            .unwrap();
        assert_eq!(mark.content, "✗ ");
        assert_eq!(mark.style.fg, Some(Color::Red));
        assert_eq!(
            lines[1].to_string(),
            "  its log: ~/.agent/state.sqlite.companions/approver-auto.log"
        );
    }

    #[test]
    fn skills_to_fix_are_one_muted_alert_naming_each() {
        let skill = |name: &str, warning| agent_client::policy::Skill {
            name: name.into(),
            path: std::path::PathBuf::from(format!("/s/{name}/SKILL.md")),
            summary: String::new(),
            warning,
        };
        assert!(skill_alerts(&[skill("good", None)]).is_empty());
        let alerts = skill_alerts(&[
            skill("good", None),
            skill("bare", Some(agent_client::policy::NO_DESCRIPTION)),
            skill("vague", Some(agent_client::policy::NO_TRIGGER)),
        ]);
        assert_eq!(
            alerts,
            [Alert {
                failed: false,
                said: "2 skills to fix, agents may not know when to read them".into(),
                detail: "bare: no description · vague: its description never says when to use it"
                    .into(),
            }]
        );
        let lines = alert_lines(&alerts[0], 40);
        let dim = |s: &Span| {
            let style = s.style;
            s.content.trim().is_empty()
                || style.add_modifier.contains(ratatui::style::Modifier::DIM)
        };
        assert!(
            lines.iter().all(|line| line
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::DIM)
                || line.spans.iter().all(dim)),
            "{lines:?}"
        );
    }

    #[test]
    fn tasks_are_grouped_by_where_they_stand() {
        assert_eq!(group("working", true, false, false, false), Group::Working);
        assert_eq!(
            group("waiting on fix", true, false, false, true),
            Group::Working
        );
        assert_eq!(
            group("needs you", true, false, false, false),
            Group::NeedsYou
        );
        // A question asked ends the turn; it still needs the person.
        assert_eq!(group("done", false, true, true, false), Group::NeedsYou);
        assert_eq!(group("failed", false, false, false, false), Group::NeedsYou);
        assert_eq!(group("done", false, false, true, false), Group::Review);
        // A goal's check passed: the pull request still waits on review.
        assert_eq!(group("checked", false, false, true, false), Group::Review);
        assert_eq!(group("stopped", false, false, true, false), Group::Done);
        assert_eq!(group("done", false, false, false, false), Group::Done);
        assert_eq!(group("failed", false, true, true, true), Group::Resolved);
    }

    #[test]
    fn a_reply_names_the_pull_request_it_links_last() {
        assert_eq!(
            pull_request(
                "Opened https://github.com/o/r/pull/12, then (https://github.com/o/r/pull/235/files)."
            ),
            Some("o/r#235".into())
        );
        assert_eq!(
            pull_request("MR: <https://gitlab.com/g/sub/r/-/merge_requests/7>"),
            Some("g/sub/r#7".into())
        );
        assert_eq!(
            pull_request("See https://github.com/o/r/issues/3 and #4"),
            None
        );
    }

    #[test]
    fn the_card_puts_the_lead_first_and_groups_its_tasks() {
        let row = |name: &str, state: &str, group: Group| Row {
            name: name.into(),
            bot: format!("calc.{name}"),
            state: state.into(),
            running: matches!(group, Group::Working | Group::NeedsYou),
            seconds: Some(42),
            model: "chatgpt/gpt-6.1-sol".into(),
            place: "tab 1".into(),
            group,
            ..Row::default()
        };
        let rows = [
            Row {
                profile: "superlead".into(),
                ..row("lead", "working", Group::Working)
            },
            row("zero", "needs you", Group::NeedsYou),
            Row {
                profile: "task".into(),
                pr: "o/r#9".into(),
                background: "↻ 1 automation".into(),
                ..row("equal", "done", Group::Review)
            },
            row("old", "done", Group::Resolved),
        ];
        let text = |resolved| -> Vec<String> {
            card("calc", &rows, 100, resolved)
                .0
                .iter()
                .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
                .collect()
        };
        let folded = text(false);
        assert_eq!(folded[0], "calc · 3 tasks · 1 need you · 1 to review");
        assert_eq!(folded[1], " ● lead working · 42s");
        // The profile is said even when it is the seat's own.
        assert_eq!(
            folded[2],
            "   GPT 6.1 Sol · chatgpt · superlead profile · tab 1"
        );
        assert_eq!(folded[4], "Needs you · 1");
        assert_eq!(folded[5], " ? zero needs you · 42s");
        assert_eq!(folded[8], "Ready for review · 1");
        assert_eq!(folded[9], " ◆ equal done · took 42s");
        assert_eq!(
            folded[10],
            "   GPT 6.1 Sol · chatgpt · task profile · tab 1 · o/r#9 · ↻ 1 automation"
        );
        assert_eq!(folded.last().unwrap(), "1 resolved · r shows them");
        let (_, named) = card("calc", &rows, 100, false);
        let named: Vec<(usize, &str)> = named.iter().map(|(at, n)| (*at, n.as_str())).collect();
        assert_eq!(
            named,
            [(1, "calc.lead"), (5, "calc.zero"), (9, "calc.equal")]
        );
        let shown = text(true);
        assert_eq!(shown[shown.len() - 3], "Resolved · 1");
        assert_eq!(shown[shown.len() - 2], " ✓ old done · took 42s");
    }
}
