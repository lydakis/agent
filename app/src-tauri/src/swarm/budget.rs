//! Budget reporting is app policy. Per-member checks avoid hiding an
//! exhausted worker behind the unused allowances of idle peers.
use super::*;

pub fn member(bot: &Value) -> Value {
    let used = bot["tokens_used"].as_u64().unwrap_or(0);
    let cap = bot["budget_tokens"].as_u64();
    json!({"name": bot["name"], "bot_id": bot["bot_id"], "status": bot["status"],
        "running_turn": bot["running_turn"], "used": used, "limit": cap,
        "remaining": cap.map(|cap| cap.saturating_sub(used))})
}

pub fn warnings(state: &mut State, members: &[Value], at: u64) -> Vec<(Value, Notice)> {
    let mut out = Vec::new();
    for member in members {
        let (Some(cap), Some(id), Some(name)) = (
            member["limit"].as_u64().filter(|n| *n > 0),
            member["bot_id"].as_i64(),
            member["name"].as_str(),
        ) else {
            continue;
        };
        // A warning cannot rescue a finished/exhausted turn and must not wake it.
        if member["running_turn"].as_i64().is_none() {
            continue;
        }
        let used = member["used"].as_u64().unwrap_or(0);
        let percent = u128::from(used) * 100 / u128::from(cap);
        let Some(level) = [50u8, 65, 80]
            .into_iter()
            .rfind(|p| u128::from(*p) <= percent)
        else {
            continue;
        };
        let previous = state.warned.entry(id.to_string()).or_default();
        if level <= *previous {
            continue;
        }
        *previous = level;
        let text = format!(
            "{name}: {used} of {cap} lifetime tokens used; {} remain. Cached input is counted on every call.",
            cap.saturating_sub(used)
        );
        out.push((
            json!({"at": at, "from": "budget", "member": name, "spent": level, "text": text}),
            Notice {
                prompt: format!("[board] budget: {text}"),
                audience: Audience::Budget {
                    member: name.to_owned(),
                    turn: member["running_turn"].as_i64().unwrap(),
                },
            },
        ));
    }
    out
}

/// A swarm with nothing running and no final result stays so until someone
/// wakes it: out of budget, failed or stalled. Its coordinator hears once,
/// and again only after something has run since.
pub fn quiet(s: &Swarm, state: &mut State, scan: &Scan, at: u64) -> Option<(Value, Notice)> {
    if !scan.listed || scan.members.is_empty() {
        return None;
    }
    if scan
        .members
        .iter()
        .any(|m| m["running_turn"].as_i64().is_some())
    {
        state.told = false;
        return None;
    }
    if s.coordinator.is_none() || state.result.is_some() || state.told {
        return None;
    }
    state.told = true;
    let outcome = status(s, state, scan, None)["outcome"].clone();
    let outcome = outcome.as_str().unwrap_or_default();
    let text = format!("nothing is running and there is no final result ({outcome})");
    Some((
        json!({"at": at, "from": "swarm", "kind": "quiet", "outcome": outcome, "text": text}),
        Notice {
            prompt: format!("[swarm {}] {text}.", s.name),
            audience: Audience::Coordinator,
        },
    ))
}

pub fn status(s: &Swarm, state: &State, scan: &Scan, last_activity: Option<u64>) -> Value {
    let used = scan.used(state);
    let missing_members: Vec<_> = s
        .members
        .iter()
        .filter(|name| !scan.members.iter().any(|m| m["name"] == name.as_str()))
        .collect();
    // Helpers have small caps of their own, and one member out of tokens
    // leaves the others working: the swarm is exhausted when its total is,
    // or when no member can go on.
    let members: Vec<_> = (scan.members.iter())
        .filter(|m| m["helper"] != true)
        .collect();
    let spent = |m: &Value| {
        m["limit"]
            .as_u64()
            .is_some_and(|cap| m["used"].as_u64().unwrap_or(0) >= cap)
    };
    let exhausted_members: Vec<_> = (members.iter())
        .filter(|m| spent(m))
        .map(|m| &m["name"])
        .collect();
    let exhausted = !members.is_empty() && exhausted_members.len() == members.len();
    let running = scan
        .members
        .iter()
        .any(|m| m["running_turn"].as_i64().is_some());
    let failed = members.iter().any(|m| m["status"] == "failed");
    let missing = state.tasks.values().any(|t| {
        let needed: &[&str] = match t["status"].as_str() {
            Some("reviewed") => &[],
            Some("reviewing") => &["reviewer"],
            _ => &["owner", "reviewer"],
        };
        needed
            .iter()
            .any(|k| !s.members.iter().any(|m| Some(s.short(m)) == t[*k].as_str()))
    });
    let outcome = if state.result.is_some() {
        "completed"
    } else if s.stopped {
        "stopped"
    } else if exhausted || used >= s.budget_tokens {
        "budget_exhausted"
    } else if missing || !missing_members.is_empty() {
        "blocked"
    } else if failed {
        "failed"
    } else if running {
        "running"
    } else {
        "partial"
    };
    json!({"swarm": s.name, "outcome": outcome, "members": scan.members, "tokens_used": used,
        "budget_tokens": s.budget_tokens, "remaining_tokens": s.budget_tokens.saturating_sub(used),
        "tasks": state.tasks, "result": state.result, "missing_members": missing_members,
        "exhausted_members": exhausted_members, "last_board_activity_ms": last_activity})
}

#[cfg(test)]
mod tests {
    use super::super::tests::swarm;
    use super::*;
    #[test]
    fn warns_an_individual_early_once_without_waking_idle_members() {
        let mut state = State::default();
        let mut m = json!({"name":"agent.x-1", "bot_id": 10, "limit": 1_000_000, "used": 510_000, "running_turn": 1});
        let warnings = warnings(&mut state, &[m.clone()], 1);
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].0["text"]
                .as_str()
                .unwrap()
                .contains("490000 remain")
        );
        assert!(super::warnings(&mut state, &[m.clone()], 2).is_empty());
        m["used"] = json!(670_000);
        assert_eq!(
            super::warnings(&mut state, &[m.clone()], 3)[0].0["spent"],
            65
        );
        m["used"] = json!(810_000);
        m["running_turn"] = Value::Null;
        assert!(super::warnings(&mut state, &[m], 4).is_empty());
    }
    #[test]
    fn status_distinguishes_partial_exhaustion_and_real_completion() {
        let s = swarm(&["agent.x-1"]);
        let mut state = State::default();
        state.tasks.insert(
            "audit".into(),
            json!({"owner":"x-1","reviewer":"x-1","result":"partial evidence"}),
        );
        let mut scan = Scan {
            members: vec![json!({"name":"agent.x-1","limit":1000,"used":200,"status":"completed"})],
            ..Scan::default()
        };
        assert_eq!(status(&s, &state, &scan, Some(42))["outcome"], "partial");
        scan.members[0]["used"] = json!(1001);
        let result = status(&s, &state, &scan, Some(42));
        assert_eq!(result["outcome"], "budget_exhausted");
        assert_eq!(result["tasks"]["audit"]["result"], "partial evidence");
        state.result = Some(
            json!({"outcome": "partial", "summary": "Deliverable incomplete; reviewed evidence available"}),
        );
        let final_status = status(&s, &state, &scan, Some(42));
        assert_eq!(final_status["outcome"], "completed");
        assert_eq!(final_status["result"]["outcome"], "partial");
    }
    #[test]
    fn only_participants_needed_for_unfinished_work_can_block_the_swarm() {
        for phase in ["assigned", "working", "reviewing", "reviewed"] {
            for absent in ["owner", "reviewer", "both"] {
                let mut names = vec!["agent.x-1"];
                if absent == "owner" {
                    names.push("agent.x-3");
                } else if absent == "reviewer" {
                    names.push("agent.x-2");
                }
                let s = swarm(&names);
                let mut state = State::default();
                state.tasks.insert(
                    "audit".into(),
                    json!({
                        "owner": "x-2", "reviewer": "x-3", "status": phase,
                        "result": "Recorded evidence", "verdict": "supported",
                    }),
                );
                let scan = Scan {
                    members: names
                        .iter()
                        .map(|name| {
                            json!({
                                "name": name, "limit": 1000, "used": 10, "running_turn": 7,
                            })
                        })
                        .collect(),
                    ..Scan::default()
                };
                let expected = if phase == "reviewed" || (phase == "reviewing" && absent == "owner")
                {
                    "running"
                } else {
                    "blocked"
                };
                let result = status(&s, &state, &scan, None);
                assert_eq!(result["outcome"], expected, "{phase}: missing {absent}");
                assert_eq!(result["tasks"]["audit"]["result"], "Recorded evidence");
            }
        }
    }

    #[test]
    fn a_spent_helper_or_one_spent_member_does_not_exhaust_the_swarm() {
        let s = swarm(&["agent.x-1", "agent.x-2"]);
        let at =
            |used: u64, turn: Value| json!({"limit": 1000, "used": used, "running_turn": turn});
        let mut scan = Scan {
            members: vec![at(1000, Value::Null), at(10, json!(4)), at(50, Value::Null)],
            ..Scan::default()
        };
        for (m, name) in scan
            .members
            .iter_mut()
            .zip(["agent.x-1", "agent.x-2", "agent.x-1.ask"])
        {
            m["name"] = json!(name);
        }
        scan.members[2]["helper"] = json!(true);
        scan.members[2]["limit"] = json!(50);
        let running = status(&s, &State::default(), &scan, None);
        assert_eq!(running["outcome"], "running");
        assert_eq!(running["exhausted_members"], json!(["agent.x-1"]));
        scan.members[1]["used"] = json!(1000);
        scan.members[1]["running_turn"] = Value::Null;
        assert_eq!(
            status(&s, &State::default(), &scan, None)["outcome"],
            "budget_exhausted"
        );
    }
}
