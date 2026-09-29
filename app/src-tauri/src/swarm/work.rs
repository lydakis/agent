//! App-owned work contracts. Assignment, claim, handoff and review share
//! the board lock; an acknowledgment never substitutes for a state change.
use super::*;

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Assign {
        task: String,
        owner: String,
        reviewer: String,
        brief: String,
    },
    Claim(String),
    Submit {
        task: String,
        result: String,
    },
    Review {
        task: String,
        verdict: String,
        evidence: String,
    },
    Finish {
        outcome: String,
        summary: String,
    },
    Leave,
}

pub fn parse(words: &[String]) -> Result<Action, String> {
    let get = |i: usize| {
        words.get(i).cloned().ok_or_else(|| {
            "invalid_work: missing argument; run the script without arguments for usage".to_owned()
        })
    };
    let rest = |i: usize| words.get(i..).unwrap_or_default().join(" ");
    match words.first().map(String::as_str) {
        Some("--assign") => Ok(Action::Assign {
            task: get(1)?,
            owner: get(2)?,
            reviewer: get(3)?,
            brief: rest(4),
        }),
        Some("--claim") => Ok(Action::Claim(get(1)?)),
        Some("--submit") => Ok(Action::Submit {
            task: get(1)?,
            result: rest(2),
        }),
        Some("--review") => Ok(Action::Review {
            task: get(1)?,
            verdict: get(2)?,
            evidence: rest(3),
        }),
        Some("--finish") => Ok(Action::Finish {
            outcome: get(1)?,
            summary: rest(2),
        }),
        Some("--leave") => Ok(Action::Leave),
        _ => Err("invalid_work: unknown operation".into()),
    }
}

fn text(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() || value.len() > MAX_POST {
        return Err("invalid_work: provide nonempty evidence or a brief, at most 16 KiB".into());
    }
    Ok(value.to_owned())
}

pub fn apply(
    s: &Swarm,
    state: &mut State,
    author: Option<&str>,
    action: Action,
    at: u64,
) -> Result<(Vec<Value>, Vec<Notice>, Value), String> {
    let bot = author.ok_or("agents_only: use a swarm agent for work operations")?;
    let me = s.short(bot).to_owned();
    let first = s.members.first().ok_or("no_members")?;
    let mut notices = Vec::new();
    let mut lines = Vec::new();
    let mut line = json!({"at": at, "from": me});
    let tell = |short: &str, message: String| Notice {
        prompt: format!("[board] {message}"),
        audience: Audience::Wake {
            who: s
                .members
                .iter()
                .filter(|m| s.short(m) == short)
                .cloned()
                .collect(),
            working: false,
        },
    };
    match action {
        Action::Assign {
            task,
            owner,
            reviewer,
            brief,
        } => {
            let brief = text(&brief)?;
            if task.is_empty()
                || task.len() > MAX_STREAM
                || !task
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                || !task.as_bytes()[0].is_ascii_alphanumeric()
            {
                return Err("invalid_task: use a short lowercase task name".into());
            }
            let member = |name: &str| {
                s.members
                    .iter()
                    .find(|m| m.as_str() == name || s.short(m) == name)
                    .map(|m| s.short(m).to_owned())
                    .ok_or_else(|| format!("not_a_member: {name}"))
            };
            let reviewer = member(&reviewer)?;
            if s.short(&owner) == reviewer {
                return Err("independent_review_required: choose a different reviewer".into());
            }
            if state.tasks.len() >= 128 {
                return Err("too_many_tasks: finish this swarm before starting another".into());
            }
            let gone = |name: &Value| !s.members.iter().any(|m| Some(s.short(m)) == name.as_str());
            // A submitted result outlives both participants: replacing its
            // reviewer preserves the historical author, even after departure.
            // Only new work below requires a current owner.
            if let Some(t) = state.tasks.get_mut(&task)
                && t["status"] == "reviewing"
                && t["owner"] == s.short(&owner)
                && gone(&t["reviewer"])
            {
                t["reviewer"] = json!(reviewer);
                let result = t["result"].as_str().unwrap_or_default().to_owned();
                notices.push(tell(
                    &reviewer,
                    format!("{task} submitted for your review: {result}"),
                ));
                line["kind"] = json!("assign");
                line["stream"] = json!(task);
                line["text"] = json!(format!("{reviewer} now reviews {task}. {brief}"));
                lines.push(line);
                let answer = json!({"task": task, "status": "reviewing", "completed": state.result.is_some()});
                return Ok((lines, notices, answer));
            }
            let owner = member(&owner)?;
            if let Some(old) = state.tasks.get(&task) {
                // Recovery is explicit: a released task or departed participant can be reassigned.
                let missing = gone(&old["owner"]) || gone(&old["reviewer"]);
                let changed =
                    old["owner"] != owner || old["reviewer"] != reviewer || old["brief"] != brief;
                let released = changed
                    && old["status"] == "assigned"
                    && !state.streams.values().any(|stream| stream == &task);
                if !missing && !released {
                    return Err(
                        "task_exists: use the existing assignment or a distinct follow-up task"
                            .into(),
                    );
                }
            }
            if state
                .tasks
                .iter()
                .any(|(key, t)| key != &task && t["owner"] == owner && t["status"] != "reviewed")
            {
                return Err("owner_busy: finish or review the owner's existing task before assigning another".into());
            }
            let mut next = state.clone();
            if next.tasks.contains_key(&task) {
                next.proposals.retain(|p| p.stream != task);
                next.streams.retain(|_, stream| stream != &task);
            }
            let mut proposed = if s.council > 0 {
                Some(plan(
                    s,
                    &mut next,
                    author,
                    Act::Propose {
                        stream: task.clone(),
                        why: format!("Owner {owner}; independent reviewer {reviewer}. {brief}"),
                    },
                    at,
                )?)
            } else {
                None
            };
            *state = next;
            state.tasks.insert(task.clone(), json!({"owner": owner, "reviewer": reviewer, "brief": brief, "status": "assigned", "result": null, "verdict": null, "evidence": null}));
            state.result = None;
            if let Some((more, messages, _)) = proposed.take() {
                lines.extend(more);
                notices.extend(messages);
            } else {
                notices.push(tell(&owner, format!("Assigned {task} to you; reviewer {reviewer}. {brief}\nRun claim {task} before starting.")));
            }
            line["kind"] = json!("assign");
            line["stream"] = json!(task);
            line["text"] = json!(format!("{owner} owns {task}; {reviewer} reviews. {brief}"));
        }
        Action::Claim(task) => {
            let approved = s.council == 0 || state.approved(&task);
            let t = state
                .tasks
                .get_mut(&task)
                .ok_or("no_task: read status for your assignment")?;
            if t["owner"] != me {
                return Err("task_owner_only: this task belongs to another member".into());
            }
            if !approved {
                return Err("not_approved: wait for the council's decision".into());
            }
            if t["status"] != "assigned" {
                return Err("already_claimed: this task is already working or reported".into());
            }
            t["status"] = json!("working");
            state.streams.insert(me.clone(), task.clone());
            line["kind"] = json!("claim");
            line["stream"] = json!(task);
            line["text"] = json!(format!("{me} started {task}"));
        }
        Action::Submit { task, result } => {
            let result = text(&result)?;
            let t = state.tasks.get_mut(&task).ok_or("no_task")?;
            if t["owner"] != me || t["status"] != "working" {
                return Err("task_owner_only: claim your assignment before submitting it".into());
            }
            t["result"] = json!(result);
            t["status"] = json!("reviewing");
            let reviewer = t["reviewer"].as_str().ok_or("invalid_reviewer")?;
            notices.push(tell(
                reviewer,
                format!("{task} submitted for your review: {result}"),
            ));
            state.streams.remove(&me);
            line["kind"] = json!("submit");
            line["stream"] = json!(task);
            line["text"] = json!(result);
        }
        Action::Review {
            task,
            verdict,
            evidence,
        } => {
            let evidence = text(&evidence)?;
            if !["supported", "conditional", "rejected"].contains(&verdict.as_str()) {
                return Err("invalid_verdict: supported, conditional or rejected".into());
            }
            let t = state.tasks.get_mut(&task).ok_or("no_task")?;
            if t["reviewer"] != me || t["owner"] == me || t["status"] != "reviewing" {
                return Err("reviewer_only: only the assigned independent reviewer can review a submitted result".into());
            }
            t["verdict"] = json!(verdict);
            t["evidence"] = json!(evidence);
            t["status"] = json!("reviewed");
            if let Some(p) = state
                .proposals
                .iter_mut()
                .find(|p| p.stream == task && p.status == "approved")
            {
                p.status = "completed".into();
            }
            state.streams.retain(|_, stream| stream != &task);
            notices.push(tell(
                s.short(first),
                format!("{task} reviewed: {verdict}. {evidence}"),
            ));
            line["kind"] = json!("review");
            line["stream"] = json!(task);
            line["text"] = json!(format!("{verdict}: {evidence}"));
        }
        Action::Finish { outcome, summary } => {
            let summary = text(&summary)?;
            if !["achieved", "partial", "failed"].contains(&outcome.as_str()) {
                return Err("invalid_outcome: achieved, partial or failed".into());
            }
            if state
                .result
                .as_ref()
                .is_some_and(|r| r["outcome"] == outcome && r["summary"] == summary)
            {
                return Err("already_finished: this handoff is already published".into());
            }
            // Completion policy lives in the editable profile; the harness records
            // the current declared outcome without requiring a particular workflow.
            // A revision replaces only this snapshot; earlier handoffs stay on the board.
            state.result =
                Some(json!({"outcome": outcome, "summary": summary, "by": me, "at": at}));
            // A partial handoff does not cancel work or revoke stream membership.
            notices.push(Notice {
                prompt: format!(
                    "[swarm {}] final result from {me}: {outcome}. {summary}",
                    s.name
                ),
                audience: Audience::Coordinator,
            });
            line["kind"] = json!("finish");
            line["outcome"] = json!(outcome);
            line["text"] = json!(summary);
        }
        Action::Leave => {
            let stream = state
                .streams
                .remove(&me)
                .ok_or("no_stream: you are not in a stream")?;
            if let Some(t) = state.tasks.get_mut(&stream)
                && t["owner"] == me
                && t["status"] == "working"
            {
                t["status"] = json!("assigned");
            }
            state.prune();
            line["kind"] = json!("leave");
            line["stream"] = json!(stream);
            line["text"] = json!(format!("{me} left {stream}"));
        }
    }
    let task = line["stream"].as_str();
    let answer = json!({"task": task, "status": task.and_then(|key| state.tasks.get(key)).map(|t| &t["status"]), "completed": state.result.is_some()});
    lines.push(line);
    Ok((lines, notices, answer))
}

#[cfg(test)]
mod tests {
    use super::super::tests::{agent, council, swarm};
    use super::*;

    fn assignment() -> Action {
        Action::Assign { task: "waits".into(), owner: "latency-2".into(), reviewer: "latency-3".into(), brief: "Inspect wait cleanup; deliver source evidence and a counterexample or bounded test.".into() }
    }
    fn apply(
        s: &Swarm,
        state: &mut State,
        who: usize,
        action: Action,
    ) -> Result<(Vec<Value>, Vec<Notice>, Value), String> {
        plan(s, state, Some(&agent(who)), Act::Work(action), 1)
    }
    #[test]
    fn ownership_review_and_completion_are_real_state_transitions() {
        let s = swarm(&[&agent(1), &agent(2), &agent(3)]);
        let mut state = State::default();
        // A flat peer registers its own piece without a central planner gate.
        apply(&s, &mut state, 2, assignment()).unwrap();
        let saved = state.clone();
        assert!(apply(&s, &mut state, 1, assignment()).is_err());
        assert_eq!(state, saved);
        assert!(apply(&s, &mut state, 3, Action::Claim("waits".into())).is_err());
        apply(&s, &mut state, 2, Action::Claim("waits".into())).unwrap();
        assert_eq!(state.streams["latency-2"], "waits");
        let (_, notices, _) = apply(
            &s,
            &mut state,
            2,
            Action::Submit {
                task: "waits".into(),
                result: "handles.rs:466 scans all indexes; no timing claim.".into(),
            },
        )
        .unwrap();
        assert!(!state.streams.contains_key("latency-2"));
        assert!(
            matches!(&notices[0].audience, Audience::Wake { who, working: false } if who == &vec![agent(3)])
        );
        assert!(
            apply(
                &s,
                &mut state,
                2,
                Action::Review {
                    task: "waits".into(),
                    verdict: "supported".into(),
                    evidence: "I agree".into()
                }
            )
            .is_err()
        );
        apply(&s, &mut state, 3, Action::Review { task: "waits".into(), verdict: "conditional".into(), evidence: "Verified source; does not affect swarms without helper waits. Measure matched concurrency.".into() }).unwrap();
        apply(
            &s,
            &mut state,
            1,
            Action::Finish {
                outcome: "achieved".into(),
                summary: "Conditional finding; no measured speedup.".into(),
            },
        )
        .unwrap();
        assert_eq!(state.result.as_ref().unwrap()["outcome"], "achieved");
        assert!(state.streams.is_empty());
    }
    #[test]
    fn completion_records_the_swarm_judgment_without_prescribing_a_workflow() {
        for s in [
            swarm(&[&agent(1)]),
            council(&[&agent(1), &agent(2), &agent(3)]),
        ] {
            let mut state = State::default();
            let (lines, _, _) = apply(
                &s,
                &mut state,
                1,
                Action::Finish {
                    outcome: "achieved".into(),
                    summary: "The requested answer is on the board.".into(),
                },
            )
            .unwrap();
            assert_eq!(state.result.as_ref().unwrap()["outcome"], "achieved");
            assert_eq!(lines[0]["text"], "The requested answer is on the board.");
            assert!(state.tasks.is_empty());
        }
    }
    #[test]
    fn a_partial_handoff_preserves_unfinished_work_and_is_not_success() {
        for s in [
            swarm(&[&agent(1), &agent(2), &agent(3)]),
            council(&[&agent(1), &agent(2), &agent(3)]),
        ] {
            let mut state = State::default();
            apply(&s, &mut state, 2, assignment()).unwrap();
            if s.council == 0 {
                apply(&s, &mut state, 2, Action::Claim("waits".into())).unwrap();
            }
            let saved = state.clone();
            assert!(
                apply(
                    &s,
                    &mut state,
                    3,
                    Action::Finish {
                        outcome: "done".into(),
                        summary: "unsupported outcome".into()
                    }
                )
                .is_err()
            );
            assert_eq!(state, saved);
            let finish = Action::Finish {
                outcome: "partial".into(),
                summary: "Available outputs are recorded; validation is still missing.".into(),
            };
            apply(&s, &mut state, 3, finish.clone()).unwrap();
            assert_eq!(state.tasks, saved.tasks);
            assert_eq!(state.streams, saved.streams);
            assert_eq!(state.result.as_ref().unwrap()["outcome"], "partial");
            assert!(apply(&s, &mut state, 1, finish).is_err());
        }
    }
    #[test]
    fn continued_work_can_replace_a_partial_handoff_and_keep_both_on_the_board() {
        for council_size in [0, 3] {
            let s = Swarm {
                council: council_size,
                ..swarm(&[&agent(1), &agent(2), &agent(3)])
            };
            let dir = std::env::temp_dir().join(format!(
                "agent-handoff-revision-{council_size}-{}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::File::create(dir.join("board.jsonl")).unwrap();
            let run = |who, action| {
                locked(&dir, |state| {
                    let (lines, notices, _) = plan(&s, state, Some(&agent(who)), action, 1)?;
                    Ok((lines, notices))
                })
                .unwrap()
            };
            run(1, Act::Work(assignment()));
            if council_size > 0 {
                for who in [1, 3] {
                    run(
                        who,
                        Act::Vote {
                            id: "P1".into(),
                            yes: true,
                            reason: "Distinct scope.".into(),
                        },
                    );
                }
            }
            run(2, Act::Work(Action::Claim("waits".into())));
            run(
                2,
                Act::Work(Action::Submit {
                    task: "waits".into(),
                    result: "Implementation and tests complete.".into(),
                }),
            );
            run(
                1,
                Act::Work(Action::Finish {
                    outcome: "partial".into(),
                    summary: "Independent review pending.".into(),
                }),
            );
            run(
                3,
                Act::Work(Action::Review {
                    task: "waits".into(),
                    verdict: "supported".into(),
                    evidence: "Checked implementation and ran tests.".into(),
                }),
            );
            let notices = run(
                1,
                Act::Work(Action::Finish {
                    outcome: "achieved".into(),
                    summary: "Implementation complete and independently verified.".into(),
                }),
            );
            assert!(
                notices
                    .iter()
                    .any(|n| n.audience == Audience::Coordinator && n.prompt.contains("achieved"))
            );
            let restored = State::read(&dir).unwrap();
            assert_eq!(restored.result.as_ref().unwrap()["outcome"], "achieved");
            assert_eq!(restored.tasks["waits"]["status"], "reviewed");
            assert_eq!(restored.tasks.len(), 1, "no placeholder assignment needed");
            let board = std::fs::read_to_string(dir.join("board.jsonl")).unwrap();
            let handoffs: Vec<Value> = board
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .filter(|line| line["kind"] == "finish")
                .collect();
            assert_eq!(handoffs.len(), 2);
            assert_eq!(handoffs[0]["outcome"], "partial");
            assert_eq!(handoffs[1]["outcome"], "achieved");
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn council_approves_assignment_before_owner_can_start() {
        let s = council(&[&agent(1), &agent(2), &agent(3)]);
        let mut state = State::default();
        apply(&s, &mut state, 1, assignment()).unwrap();
        assert!(
            apply(&s, &mut state, 2, Action::Claim("waits".into()))
                .unwrap_err()
                .starts_with("not_approved")
        );
        for n in [1, 3] {
            plan(
                &s,
                &mut state,
                Some(&agent(n)),
                Act::Vote {
                    id: "P1".into(),
                    yes: true,
                    reason: "Distinct scope and independent reviewer.".into(),
                },
                2,
            )
            .unwrap();
        }
        assert_eq!(state.streams.get("latency-2"), Some(&"waits".to_owned()));
        assert!(!state.streams.contains_key("latency-1"));
        apply(&s, &mut state, 2, Action::Claim("waits".into())).unwrap();
        assert!(
            plan(
                &s,
                &mut state,
                Some(&agent(3)),
                Act::Join("waits".into()),
                3
            )
            .is_err()
        );
    }
    #[test]
    fn a_denied_assignment_frees_its_owner_and_tells_its_proposer() {
        let s = council(&[&agent(1), &agent(2), &agent(3)]);
        let mut state = State::default();
        apply(&s, &mut state, 1, assignment()).unwrap();
        let mut notices = Vec::new();
        for n in [2, 3] {
            let no = Act::Vote {
                id: "P1".into(),
                yes: false,
                reason: "Overlaps the profile work.".into(),
            };
            notices = plan(&s, &mut state, Some(&agent(n)), no, 2).unwrap().1;
        }
        assert!(state.tasks.is_empty());
        assert!(
            matches!(&notices[0].audience, Audience::Wake { who, .. } if who == &vec![agent(2), agent(1)])
        );
        // The owner can take other work at once.
        let mut other = assignment();
        if let Action::Assign { task, .. } = &mut other {
            *task = "other".into();
        }
        apply(&s, &mut state, 1, other).unwrap();
    }
    #[test]
    fn a_new_reviewer_takes_over_a_submitted_result_even_after_its_owner_leaves() {
        for council_size in [0, 3] {
            for (owner_left, full_name) in [(false, false), (true, false), (true, true)] {
                let s = Swarm {
                    council: council_size,
                    ..swarm(&[&agent(1), &agent(2), &agent(3)])
                };
                let mut state = State::default();
                apply(&s, &mut state, 1, assignment()).unwrap();
                if council_size > 0 {
                    for who in [1, 3] {
                        plan(
                            &s,
                            &mut state,
                            Some(&agent(who)),
                            Act::Vote {
                                id: "P1".into(),
                                yes: true,
                                reason: "Distinct scope.".into(),
                            },
                            1,
                        )
                        .unwrap();
                    }
                }
                apply(&s, &mut state, 2, Action::Claim("waits".into())).unwrap();
                let result = "handles.rs:466 scans all indexes.";
                apply(
                    &s,
                    &mut state,
                    2,
                    Action::Submit {
                        task: "waits".into(),
                        result: result.into(),
                    },
                )
                .unwrap();
                // The reviewer leaves, optionally after the owner has left too.
                let names = if owner_left {
                    vec![agent(1), agent(4)]
                } else {
                    vec![agent(1), agent(2), agent(4)]
                };
                let s = Swarm {
                    council: council_size,
                    ..swarm(&names.iter().map(String::as_str).collect::<Vec<_>>())
                };
                let owner = if full_name {
                    agent(2)
                } else {
                    "latency-2".into()
                };
                let reassign = |task: &str, reviewer: &str| Action::Assign {
                    task: task.into(),
                    owner: owner.clone(),
                    reviewer: reviewer.into(),
                    brief: "Review the submitted result.".into(),
                };
                let saved = state.clone();
                // Recovery still requires a current independent reviewer.
                assert!(apply(&s, &mut state, 1, reassign("waits", "latency-9")).is_err());
                assert!(apply(&s, &mut state, 1, reassign("waits", "latency-2")).is_err());
                if owner_left {
                    // A historical author cannot be assigned new work.
                    assert!(apply(&s, &mut state, 1, reassign("new-work", "latency-4")).is_err());
                }
                assert_eq!(state, saved);
                let (_, notices, _) =
                    apply(&s, &mut state, 1, reassign("waits", "latency-4")).unwrap();
                let mut expected = saved.tasks["waits"].clone();
                expected["reviewer"] = json!("latency-4");
                assert_eq!(
                    state.tasks["waits"], expected,
                    "authorship, brief and result are preserved"
                );
                assert_eq!(state.proposals, saved.proposals, "no new council approval");
                assert!(
                    matches!(&notices[0].audience, Audience::Wake { who, .. } if who == &vec![agent(4)])
                );
                apply(
                    &s,
                    &mut state,
                    4,
                    Action::Review {
                        task: "waits".into(),
                        verdict: "supported".into(),
                        evidence: "Read handles.rs:466.".into(),
                    },
                )
                .unwrap();
                assert_eq!(state.tasks["waits"]["status"], "reviewed");
            }
        }
    }
    #[test]
    fn leaving_releases_membership_and_reassignment_requires_explicit_recovery() {
        let s = swarm(&[&agent(1), &agent(2), &agent(3)]);
        let mut state = State::default();
        apply(&s, &mut state, 1, assignment()).unwrap();
        apply(&s, &mut state, 2, Action::Claim("waits".into())).unwrap();
        apply(&s, &mut state, 2, Action::Leave).unwrap();
        assert!(state.streams.is_empty());
        assert_eq!(state.tasks["waits"]["status"], "assigned");
        // A second open piece for the same owner is refused, not silently duplicated.
        let mut second = assignment();
        if let Action::Assign { task, .. } = &mut second {
            *task = "other".into();
        }
        assert!(apply(&s, &mut state, 1, second).is_err());
    }
    #[test]
    fn simultaneous_claims_have_one_winner_and_results_survive_restart() {
        let s = swarm(&[&agent(1), &agent(2), &agent(3)]);
        let dir = std::env::temp_dir().join(format!("agent-work-claim-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::File::create(dir.join("board.jsonl")).unwrap();
        locked(&dir, |state| {
            let (lines, _, _) = apply(&s, state, 1, assignment())?;
            Ok((lines, ()))
        })
        .unwrap();
        let wins = std::thread::scope(|scope| {
            let jobs: Vec<_> = (0..2)
                .map(|_| {
                    scope.spawn(|| {
                        locked(&dir, |state| {
                            let (lines, _, _) = apply(&s, state, 2, Action::Claim("waits".into()))?;
                            Ok((lines, ()))
                        })
                    })
                })
                .collect();
            jobs.into_iter()
                .map(|j| j.join().unwrap().is_ok() as usize)
                .sum::<usize>()
        });
        assert_eq!(wins, 1);
        locked(&dir, |state| {
            let (lines, _, _) = apply(
                &s,
                state,
                2,
                Action::Submit {
                    task: "waits".into(),
                    result: "Partial source evidence".into(),
                },
            )?;
            Ok((lines, ()))
        })
        .unwrap();
        let restored = State::read(&dir).unwrap();
        assert_eq!(restored.tasks["waits"]["result"], "Partial source evidence");
        assert_eq!(restored.tasks["waits"]["status"], "reviewing");
        assert!(restored.streams.is_empty());
        locked(&dir, |state| {
            let (lines, _, _) = apply(
                &s,
                state,
                3,
                Action::Finish {
                    outcome: "partial".into(),
                    summary: "Evidence saved; review unfinished.".into(),
                },
            )?;
            Ok((lines, ()))
        })
        .unwrap();
        let restored = State::read(&dir).unwrap();
        assert_eq!(restored.result.as_ref().unwrap()["outcome"], "partial");
        assert_eq!(restored.tasks["waits"]["status"], "reviewing");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
