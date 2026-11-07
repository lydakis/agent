//! `sonny ask list`: the questions a lead's agents asked lately, newest
//! first, each with where it stands, so an agent about to ask sees whether
//! one of them asked it already or the person settled it. The agent reads
//! and decides; nothing here matches wording.
//!
//! The tree is the lead the calling agent's line leads to and every agent
//! made under it (`--all`: every agent). Node IDs grow along every
//! history, so an agent whose newest node is older than the last question
//! kept holds none newer and is not read, and each history is read only
//! down to that question, and no further back than a week.
use crate::asks::{Ask, Asked, State};
use agent_client::Client;
use serde_json::{Value, json};

/// How many questions one call lists.
const LISTED: usize = 20;
/// The most nodes one history page reads, as the daemon allows.
const PAGE: usize = 400;
/// How far back "lately" goes: a history is read no further, so a long
/// one that asked little costs its last week, not its whole life.
const LATELY_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// A question found, with the agent that asked it and where.
#[derive(Debug, Clone)]
pub struct Found {
    pub bot: String,
    /// Which agent was made first, when a fork's history shares the node.
    pub bot_id: i64,
    /// The node that put it, which orders questions across agents.
    pub node: i64,
    pub written_ms: Option<i64>,
    pub ask: Ask,
    pub state: State,
}

/// What `sonny ask list` was asked.
#[derive(Debug, Default, PartialEq)]
struct Asking {
    all: bool,
    before: Option<i64>,
    json: bool,
}

fn parse(args: &[String]) -> Result<Asking, String> {
    let mut asking = Asking::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--all" => asking.all = true,
            "--json" => asking.json = true,
            "--before" => {
                let node = args.next().and_then(|n| n.parse::<i64>().ok());
                asking.before = Some(node.ok_or("--before takes the number a list ended at")?);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(asking)
}

const USAGE: &str = "usage: sonny ask list [--all] [--before NODE] [--json]";

/// `sonny ask list`, run from an agent's shell (or, outside one, for
/// every agent).
pub fn cli(args: &[String]) -> i32 {
    let asking = match parse(args) {
        Ok(asking) => asking,
        Err(error) => {
            eprintln!("{error}\n{USAGE}");
            return 2;
        }
    };
    let caller = std::env::var("AGENT_BOT").ok().filter(|b| !b.is_empty());
    let listed = crate::trigger::runtime()
        .and_then(|runtime| runtime.block_on(list(caller.as_deref(), &asking)));
    match listed {
        Ok((found, next)) if asking.json => {
            let asks: Vec<Value> = found.iter().map(to_json).collect();
            println!("{}", json!({"asks": asks, "next_before": next}));
            0
        }
        Ok((found, next)) => {
            print!("{}", text(&found, next, asking.all));
            0
        }
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}

async fn list(caller: Option<&str>, asking: &Asking) -> Result<(Vec<Found>, Option<i64>), String> {
    let daemon = crate::trigger::Daemon::current()?;
    let socket = daemon.socket()?;
    let client = crate::trigger::connect(&socket, &daemon).await?;
    let bots = match (caller, asking.all) {
        (Some(caller), false) => tree(&client, caller).await?,
        _ => every(&client).await?,
    };
    newest(&client, bots, asking.before).await
}

/// The agents of `caller`'s lead's tree: the lead its line leads to, or
/// the caller when none does, and every agent made under it.
async fn tree(client: &Client, caller: &str) -> Result<Vec<Value>, String> {
    let failed = |e: agent_client::Error| format!("ask_list_unread: {e}");
    let mut record = (client.request("inspect", json!({"bot": caller})).await).map_err(failed)?;
    record["name"] = json!(caller);
    // Its makers, up to the first that leads, read once each.
    let mut makers = vec![record.clone()];
    while makers.len() < 256
        && let Some(at) = makers.last()
        && !crate::project::is_lead(at["name"].as_str().unwrap_or_default())
        && let Some(maker) = at["created_by"].as_str().map(str::to_owned)
        && let Ok(mut made_it) = client.request("inspect", json!({"bot": maker})).await
    {
        made_it["name"] = json!(maker);
        makers.push(made_it);
    }
    let maker = |at: &Value| {
        (makers.iter())
            .find(|m| m["name"] == at["created_by"])
            .cloned()
    };
    let root = crate::declared::lead_by(&record, maker).unwrap_or(record);
    let mut bots = vec![root];
    let mut at = 0;
    while at < bots.len() {
        let Some(maker) = bots[at]["bot_id"].as_i64() else {
            at += 1;
            continue;
        };
        let mut after = Value::Null;
        loop {
            let page = client
                .request(
                    "bots",
                    json!({"after": after, "limit": 256, "created_by_id": maker}),
                )
                .await
                .map_err(failed)?;
            bots.extend(page["bots"].as_array().cloned().unwrap_or_default());
            after = page["next_after"].clone();
            if after.is_null() {
                break;
            }
        }
        at += 1;
    }
    Ok(bots)
}

/// Every agent, page by page.
async fn every(client: &Client) -> Result<Vec<Value>, String> {
    let mut bots = Vec::new();
    let mut after = Value::Null;
    loop {
        let page = client
            .request("bots", json!({"after": after, "limit": 256}))
            .await
            .map_err(|e| format!("ask_list_unread: {e}"))?;
        bots.extend(page["bots"].as_array().cloned().unwrap_or_default());
        after = page["next_after"].clone();
        if after.is_null() {
            return Ok(bots);
        }
    }
}

/// The newest [`LISTED`] questions `bots` put before node `before`, and
/// where a next list starts when there may be more.
async fn newest(
    client: &Client,
    mut bots: Vec<Value>,
    before: Option<i64>,
) -> Result<(Vec<Found>, Option<i64>), String> {
    // Newest history first, so the reading can stop at the first agent
    // with nothing newer than what it keeps.
    bots.retain(|bot| bot["head"].is_i64());
    bots.sort_by_key(|bot| std::cmp::Reverse(bot["head"].as_i64()));
    let mut kept: Vec<Found> = Vec::new();
    let mut more = false;
    for bot in &bots {
        let floor = floor(&kept);
        if floor.is_some_and(|floor| bot["head"].as_i64() < Some(floor)) {
            more = true;
            break;
        }
        let name = bot["name"].as_str().unwrap_or_default();
        let (found, past) = asked(client, name, floor, before).await?;
        more |= past;
        for mut found in found {
            found.bot_id = bot["bot_id"].as_i64().unwrap_or(i64::MAX);
            keep(&mut kept, found);
        }
        more |= trim(&mut kept);
    }
    let next = more.then(|| kept.last().map(|found| found.node)).flatten();
    Ok((kept, next))
}

/// `kept` newest first and cut to [`LISTED`] questions, and whether any
/// were cut. A node's questions stay together, one more or less, as the
/// next list starts before a node: an Anthropic message holds every call
/// it made.
fn trim(kept: &mut Vec<Found>) -> bool {
    kept.sort_by_key(|found| std::cmp::Reverse(found.node));
    let Some(edge) = kept.get(LISTED - 1).map(|found| found.node) else {
        return false;
    };
    let whole = LISTED
        + (kept[LISTED..].iter())
            .take_while(|f| f.node == edge)
            .count();
    let cut = whole < kept.len();
    kept.truncate(whole);
    cut
}

/// Keeps `found` in `kept`, once: a fork, a side chat among them, shares
/// its source's history up to where it forked, so the same question at
/// the same node is the one agent's that was made first, who asked it.
fn keep(kept: &mut Vec<Found>, found: Found) {
    let same = (kept.iter_mut()).find(|at| at.node == found.node && at.ask.id == found.ask.id);
    match same {
        Some(at) if found.bot_id < at.bot_id => *at = found,
        Some(_) => {}
        None => kept.push(found),
    }
}

/// The node of the oldest question kept once the list is full: nothing
/// older makes it.
fn floor(kept: &[Found]) -> Option<i64> {
    (kept.len() >= LISTED)
        .then(|| kept.last().map(|found| found.node))
        .flatten()
}

/// The questions `bot` put from node `floor` on and before node `before`,
/// newest first, about [`LISTED`] and a node's whole, and whether it holds
/// more. The floor's own node is read too: a fork read first may keep a
/// question it shares with its source, which [`keep`] gives the source.
async fn asked(
    client: &Client,
    bot: &str,
    floor: Option<i64>,
    before: Option<i64>,
) -> Result<(Vec<Found>, bool), String> {
    let failed = |e: agent_client::Error| format!("ask_list_unread: {e}");
    let since = now_ms() - LATELY_MS;
    let mut reading = Asked::default();
    let mut found = Vec::new();
    let mut from = Value::Null;
    loop {
        let page = client
            .request(
                "history_nodes",
                json!({"bot": bot, "from": from, "limit": PAGE, "min_node": floor}),
            )
            .await
            .map_err(failed)?;
        let nodes = page["nodes"].as_array().cloned().unwrap_or_default();
        // Newest first: the nodes written lately, up to the first before.
        let ids: Vec<i64> = (nodes.iter())
            .take_while(|n| n["written_ms"].as_i64().is_some_and(|ms| ms >= since))
            .filter_map(|n| n["node"].as_i64())
            .collect();
        let older = ids.len() < nodes.len();
        let mut next = 0;
        while next < ids.len() {
            let chunk = &ids[next..(next + 100).min(ids.len())];
            let read = (client.request("history_items", json!({"bot": bot, "nodes": chunk})))
                .await
                .map_err(failed)?;
            // Fewer than asked when they are large; at least one.
            let items = read["items"].as_array().cloned().unwrap_or_default();
            if items.is_empty() {
                return Ok((found, false));
            }
            next += items.len();
            for entry in &items {
                let Some(node) = entry["node"].as_i64() else {
                    continue;
                };
                let listed = nodes.iter().find(|n| n["node"] == node);
                let Some(turn) = listed.and_then(|n| n["turn"].as_i64()) else {
                    continue;
                };
                let asks = reading.entry(turn, entry);
                if before.is_some_and(|before| node >= before) || asks.is_empty() {
                    continue;
                }
                // A node's questions stay together, as the next list
                // starts before a node.
                if found.len() >= LISTED {
                    return Ok((found, true));
                }
                for (ask, state) in asks {
                    found.push(Found {
                        bot: bot.to_owned(),
                        bot_id: i64::MAX,
                        node,
                        written_ms: listed.and_then(|n| n["written_ms"].as_i64()),
                        ask,
                        state,
                    });
                }
            }
        }
        from = page["next_from"].clone();
        if from.is_null() || older {
            return Ok((found, false));
        }
    }
}

/// Where `found`'s question stands, in words: what an agent weighs.
fn stands(found: &Found) -> String {
    let label = |option: usize| (found.ask.options.get(option)).map(|choice| choice.label.clone());
    match &found.state {
        State::Open => "open".into(),
        State::Aside => "set aside: the person wrote past it; a pick still answers it".into(),
        State::Answered(answer) => format!("answered: {answer}"),
        State::Recorded { option, note } => {
            let going = option.and_then(label).map(|l| format!(", going with {l}"));
            format!(
                "answered in their words{}: {note}",
                going.unwrap_or_default()
            )
        }
        State::Withdrawn(note) => format!("withdrawn: {note}"),
    }
}

/// How long ago `ms` was, in its largest unit.
fn ago(ms: i64, now: i64) -> String {
    let minutes = (now - ms).max(0) / 60_000;
    match minutes {
        0 => "now".into(),
        m if m < 60 => format!("{m}m ago"),
        m if m < 48 * 60 => format!("{}h ago", m / 60),
        m => format!("{}d ago", m / (24 * 60)),
    }
}

fn now_ms() -> i64 {
    (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH))
        .map_or(0, |d| d.as_millis() as i64)
}

/// The list as an agent reads it: per question its agent, ID, age and
/// where it stands, then the question and its options.
fn text(found: &[Found], next: Option<i64>, all: bool) -> String {
    if found.is_empty() {
        return "No questions asked lately.\n".into();
    }
    let now = now_ms();
    let mut out = String::new();
    for found in found {
        let when = found.written_ms.map(|ms| format!("  {}", ago(ms, now)));
        out += &format!(
            "{}  {}{}  {}\n  {}\n",
            found.bot,
            found.ask.id,
            when.unwrap_or_default(),
            stands(found),
            found.ask.question
        );
        if !found.ask.options.is_empty() {
            let options: Vec<String> = (found.ask.options.iter().enumerate())
                .map(|(at, choice)| format!("{} {}", at + 1, choice.label))
                .collect();
            let recommended = found
                .ask
                .recommended
                .map(|r| format!(" (recommends {})", r + 1));
            out += &format!(
                "  {}{}\n",
                options.join(" · "),
                recommended.unwrap_or_default()
            );
        }
        if let Some(command) = &found.ask.command {
            out += &format!("  run: {command}\n");
        }
        // A yes covers that action, target and revision alone.
        if let Some(covers) = &found.ask.covers {
            out += &format!("  covers: {}\n", covers.line());
        }
    }
    if let Some(next) = next {
        let all = if all { " --all" } else { "" };
        out += &format!("More: sonny ask list{all} --before {next}\n");
    }
    out
}

fn to_json(found: &Found) -> Value {
    let (state, detail) = match &found.state {
        State::Open => ("open", json!({})),
        State::Aside => ("set_aside", json!({})),
        State::Answered(answer) => ("answered", json!({"answer": answer})),
        State::Recorded { option, note } => (
            "answered",
            json!({"in_their_words": true, "option": option.map(|o| o + 1), "note": note}),
        ),
        State::Withdrawn(note) => ("withdrawn", json!({"note": note})),
    };
    let mut asked = json!({
        "bot": found.bot,
        "id": found.ask.id,
        "node": found.node,
        "written_ms": found.written_ms,
        "question": found.ask.question,
        "why": found.ask.why,
        "options": (found.ask.options.iter())
            .map(|choice| json!({"label": choice.label, "explanation": choice.explanation}))
            .collect::<Vec<_>>(),
        "recommended": found.ask.recommended.map(|r| r + 1),
        "command": found.ask.command,
        "covers": found.ask.covers.as_ref().map(|covers| json!({"action": covers.action,
            "target": covers.target, "revision": covers.revision})),
        "state": state,
    });
    if let (Some(asked), Some(detail)) = (asked.as_object_mut(), detail.as_object()) {
        asked.extend(detail.clone());
    }
    asked
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asks::Choice;

    fn found(bot: &str, node: i64, state: State) -> Found {
        let choice = |label: &str| Choice {
            label: label.into(),
            explanation: String::new(),
        };
        Found {
            bot: bot.into(),
            bot_id: node,
            node,
            written_ms: Some(now_ms() - 3 * 3_600_000),
            ask: Ask {
                id: format!("ask:{node}/c1"),
                question: "Bump the major version?".into(),
                options: vec![choice("Bump it"), choice("Keep 0.x")],
                recommended: Some(0),
                why: String::new(),
                command: None,
                reason: String::new(),
                unanswered: None,
                deadline: None,
                covers: None,
                task: None,
            },
            state,
        }
    }

    /// A helper's tree is its lead's: up through its makers by the ids
    /// they made it with, then every agent made under the lead.
    #[tokio::test]
    async fn a_helpers_tree_is_its_leads_whole() {
        let client = crate::tests::daemon_answering(|request| {
            let bot = |name: &str, id: i64, by: Option<(&str, i64)>| {
                json!({"bot_id": id, "name": name, "created_by": by.map(|(b, _)| b),
                    "created_by_id": by.map(|(_, i)| i)})
            };
            match (request["op"].as_str(), request["bot"].as_str()) {
                (Some("inspect"), Some("shop.check")) => bot("x", 3, Some(("shop.fix", 2))),
                (Some("inspect"), Some("shop.fix")) => bot("x", 2, Some(("shop.lead", 1))),
                (Some("inspect"), Some("shop.lead")) => bot("x", 1, None),
                (Some("bots"), _) => match request["created_by_id"].as_i64() {
                    Some(1) => json!({"bots": [bot("shop.fix", 2, Some(("shop.lead", 1))),
                        bot("shop.docs", 4, Some(("shop.lead", 1)))], "next_after": null}),
                    Some(2) => json!({"bots": [bot("shop.check", 3, Some(("shop.fix", 2)))],
                        "next_after": null}),
                    _ => json!({"bots": [], "next_after": null}),
                },
                _ => json!({}),
            }
        })
        .await;
        let mut names: Vec<String> = (tree(&client, "shop.check").await.unwrap().iter())
            .filter_map(|bot| bot["name"].as_str().map(str::to_owned))
            .collect();
        names.sort();
        assert_eq!(names, ["shop.check", "shop.docs", "shop.fix", "shop.lead"]);
    }

    #[test]
    fn a_list_says_who_asked_what_and_where_it_stands() {
        let listed = [
            found("calc.fix", 12, State::Open),
            found(
                "calc.lead",
                9,
                State::Recorded {
                    option: Some(1),
                    note: "they said keep it".into(),
                },
            ),
            found("calc.lead", 7, State::Withdrawn("no longer needed".into())),
        ];
        let text = text(&listed, Some(7), false);
        assert!(text.starts_with("calc.fix  ask:12/c1  3h ago  open\n  Bump the major version?\n  1 Bump it · 2 Keep 0.x (recommends 1)\n"), "{text}");
        assert!(text.contains("calc.lead  ask:9/c1  3h ago  answered in their words, going with Keep 0.x: they said keep it\n"), "{text}");
        assert!(text.contains("withdrawn: no longer needed\n"), "{text}");
        assert!(
            text.ends_with("More: sonny ask list --before 7\n"),
            "{text}"
        );
        assert_eq!(text_of_none(), "No questions asked lately.\n");
        let asked = to_json(&listed[1]);
        assert_eq!(asked["state"], "answered");
        assert_eq!(asked["in_their_words"], true);
        assert_eq!(asked["option"], 2);
        assert_eq!(asked["recommended"], 1);
        assert_eq!(to_json(&found("x", 1, State::Aside))["state"], "set_aside");
        // A list of every agent goes on as one; a yes says what it covers.
        assert!(
            super::text(&listed, Some(7), true)
                .ends_with("More: sonny ask list --all --before 7\n")
        );
        let mut approving = found("calc.fix", 12, State::Answered("Bump it".into()));
        approving.ask.covers = Some(crate::asks::Covers {
            action: "merge".into(),
            target: "calc#4".into(),
            revision: Some("3f2a9c1".into()),
        });
        let shown = super::text(std::slice::from_ref(&approving), None, false);
        assert!(
            shown.contains("  covers: merge · calc#4 · at 3f2a9c1\n"),
            "{shown}"
        );
        assert_eq!(
            to_json(&approving)["covers"],
            json!({"action": "merge", "target": "calc#4", "revision": "3f2a9c1"})
        );
    }

    /// An entry held, by node and turn.
    type Held = (i64, i64, Value);

    /// A stand-in daemon holding `histories`: per agent its record and its
    /// entries by node, newest first, each with its turn.
    async fn daemon_holding(histories: Vec<(Value, Vec<Held>)>) -> std::sync::Arc<Client> {
        crate::tests::daemon_answering(move |request| {
            let held = (histories.iter()).find(|(bot, _)| bot["name"] == request["bot"]);
            match request["op"].as_str() {
                Some("bots") => {
                    json!({"bots": histories.iter().map(|(bot, _)| bot.clone()).collect::<Vec<_>>(),
                        "next_after": null})
                }
                Some("history_nodes") => {
                    let least = request["min_node"].as_i64().unwrap_or(0);
                    let nodes: Vec<Value> = (held.map(|(_, h)| h.as_slice()).unwrap_or_default())
                        .iter()
                        .filter(|(node, ..)| *node >= least)
                        .map(|(node, turn, _)| {
                            json!({"node": node, "turn": turn,
                            "written_ms": now_ms()})
                        })
                        .collect();
                    json!({"nodes": nodes, "next_from": null})
                }
                Some("history_items") => {
                    let wanted = request["nodes"].as_array().cloned().unwrap_or_default();
                    let items: Vec<Value> = (held.map(|(_, h)| h.as_slice()).unwrap_or_default())
                        .iter()
                        .filter(|(node, ..)| wanted.contains(&json!(node)))
                        .map(|(node, _, entry)| {
                            let mut entry = entry.clone();
                            entry["node"] = json!(node);
                            entry
                        })
                        .collect();
                    json!({"items": items})
                }
                _ => json!({}),
            }
        })
        .await
    }

    fn arguments() -> Value {
        json!({"question": "Bump it?", "why": "w", "if_unanswered": "wait",
            "options": [{"label": "Bump it", "explanation": "e"},
                        {"label": "Keep 0.x", "explanation": "e"}]})
    }

    /// An `ask` call and its result, at `node` and the node after, as an
    /// entry each, newest first.
    fn asking(node: i64, turn: i64, call: &str) -> Vec<Held> {
        vec![
            (
                node + 1,
                turn,
                json!({"item": {"type": "function_call_output", "call_id": call,
                "output": json!({"id": call}).to_string()}}),
            ),
            (
                node,
                turn,
                json!({"item": {"type": "function_call", "name": crate::asks::TOOL,
                "call_id": call, "arguments": arguments().to_string()}}),
            ),
        ]
    }

    /// Two questions one message put share its node: a list cut there
    /// keeps both, and the next starts past them.
    #[tokio::test]
    async fn a_list_keeps_a_nodes_questions_together() {
        let mut history = Vec::new();
        for n in (0..19).rev() {
            history.extend(asking(100 + 2 * n, 100 + n, "c"));
        }
        let both = json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": "p", "name": crate::asks::TOOL, "input": arguments()},
            {"type": "tool_use", "id": "q", "name": crate::asks::TOOL, "input": arguments()}]});
        let results = json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "p", "content": "{}"},
            {"type": "tool_result", "tool_use_id": "q", "content": "{}"}]});
        history.push((51, 50, json!({"item": results})));
        history.push((50, 50, json!({"item": both})));
        history.extend(asking(10, 10, "o"));
        let bot = json!({"name": "calc.lead", "bot_id": 1, "head": 137});
        let client = daemon_holding(vec![(bot.clone(), history)]).await;
        let (found, next) = newest(&client, vec![bot.clone()], None).await.unwrap();
        let ids: Vec<&str> = found.iter().map(|f| f.ask.id.as_str()).collect();
        assert_eq!(ids.len(), 21);
        assert_eq!(&ids[19..], ["ask:50/p", "ask:50/q"]);
        assert_eq!(next, Some(50));
        let (found, next) = newest(&client, vec![bot], next).await.unwrap();
        let ids: Vec<&str> = found.iter().map(|f| f.ask.id.as_str()).collect();
        assert_eq!((ids, next), (vec!["ask:10/o"], None));
    }

    /// A fork read first, its list full down to a question it shares with
    /// its source, still gives that question to the source, as answered
    /// there after the fork.
    #[tokio::test]
    async fn a_fork_read_first_leaves_its_sources_question_to_the_source() {
        let shared = asking(1, 1, "s");
        let mut fork = Vec::new();
        for n in (0..19).rev() {
            fork.extend(asking(10 + 2 * n, 10 + n, "f"));
        }
        fork.extend(shared.clone());
        let ask = crate::asks::call("ask:1/s", &arguments()).unwrap();
        let answer = json!({"from": null, "origin": null, "item": {"role": "user",
            "content": [{"type": "input_text", "text": crate::asks::answer(&ask, "2")}]}});
        let mut source = vec![(5, 5, answer)];
        source.extend(shared);
        let bots = vec![
            json!({"name": "calc.side", "bot_id": 5, "head": 47}),
            json!({"name": "calc.lead", "bot_id": 1, "head": 5}),
        ];
        let client = daemon_holding(vec![(bots[0].clone(), fork), (bots[1].clone(), source)]).await;
        let (found, _) = newest(&client, bots, None).await.unwrap();
        let last = found.last().unwrap();
        assert_eq!(found.len(), 20);
        assert_eq!(
            (last.bot.as_str(), last.ask.id.as_str()),
            ("calc.lead", "ask:1/s")
        );
        assert_eq!(last.state, State::Answered("Keep 0.x".into()));
    }

    fn text_of_none() -> String {
        text(&[], None, false)
    }

    #[test]
    fn a_full_list_reads_nothing_older_than_its_last() {
        let kept: Vec<Found> = (0..LISTED as i64)
            .map(|n| found("a", 100 - n, State::Open))
            .collect();
        assert_eq!(floor(&kept), Some(100 - LISTED as i64 + 1));
        assert_eq!(floor(&kept[1..]), None);
    }

    #[test]
    fn a_question_a_fork_shares_is_its_first_agents() {
        let mut kept = Vec::new();
        let mut copy = found("calc.side", 9, State::Open);
        copy.bot_id = 7;
        keep(&mut kept, copy);
        let mut source = found("calc.lead", 9, State::Answered("yes".into()));
        source.bot_id = 1;
        keep(&mut kept, source);
        let mut later = found("calc.other", 9, State::Open);
        later.bot_id = 8;
        keep(&mut kept, later);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].bot, "calc.lead");
        assert_eq!(kept[0].state, State::Answered("yes".into()));
    }

    #[test]
    fn list_takes_its_flags() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(parse(&[]), Ok(Asking::default()));
        assert_eq!(
            parse(&args(&["--all", "--before", "42", "--json"])),
            Ok(Asking {
                all: true,
                before: Some(42),
                json: true
            })
        );
        assert!(parse(&args(&["--before"])).is_err());
        assert!(parse(&args(&["--soon"])).is_err());
    }
}
