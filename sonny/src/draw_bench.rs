//! What the window itself spends to lay out a page at 50, 5,000 and
//! 10,000 agents, with no daemon and no terminal: the part of a load the
//! end-to-end bench (`bench/ui_loads.py`) cannot tell from its reads.
//! Ignored, so `cargo test` passes them by; run them on a release build:
//!
//!     cargo test --release -p sonny draw_bench -- --ignored --nocapture --test-threads 1
//!
//! Each prints, per size, the median and the slowest of its repeats in
//! microseconds, one line each, as `draw_bench NAME AGENTS p50_us max_us`.
use crate::{projects, status, switch};
use serde_json::{Value, json};
use std::time::Instant;

/// The sizes, as projects of agents: a lead and its tasks.
const SIZES: [(usize, usize); 3] = [(5, 10), (50, 100), (100, 100)];
const REPEATS: usize = 21;
const NOW: u64 = 1_800_000_000_000;

/// The rows `/status` reads for project `p`: its lead and its tasks,
/// every third closed, three working.
fn rows(p: usize, per: usize) -> Vec<status::Row> {
    (0..per)
        .map(|k| {
            let (name, bot) = match k {
                0 => ("lead".to_owned(), format!("p{p}.lead")),
                k => (format!("t{}", k - 1), format!("p{p}.t{}", k - 1)),
            };
            let state = match k {
                0 => "idle",
                k if (k - 1) % 3 == 0 => "done",
                1..=4 => "working",
                _ => "idle",
            };
            let running = state == "working";
            status::Row {
                name,
                bot,
                title: if k == 0 {
                    String::new()
                } else {
                    format!("Task {k} of project {p}")
                },
                state: state.to_owned(),
                running,
                seconds: Some(42),
                at: Some(NOW - k as u64 * 1000),
                model: "Synthetic Model".to_owned(),
                profile: if k == 0 {
                    "lead".to_owned()
                } else {
                    "task".to_owned()
                },
                latest: "done".to_owned(),
                maker: if k == 0 {
                    String::new()
                } else {
                    format!("p{p}.lead")
                },
                group: status::group(state, running, false, state == "done"),
                ..Default::default()
            }
        })
        .collect()
}

/// The daemon's `bots` rows for every agent, as `/switch` reads them.
fn bots(projects: usize, per: usize) -> Vec<Value> {
    let mut out = Vec::with_capacity(projects * per);
    for p in 0..projects {
        for k in 0..per {
            let name = match k {
                0 => format!("p{p}.lead"),
                k => format!("p{p}.t{}", k - 1),
            };
            out.push(json!({
                "name": name,
                "workspace": format!("/tmp/w/p{p}"),
                "status": "idle",
                "running_turn": if (1..=4).contains(&k) { json!({"turn": 2}) } else { Value::Null },
                "last": {"turn": 1, "status": "completed"},
                "created_by": if k == 0 { Value::Null } else { json!(format!("p{p}.lead")) },
            }));
        }
    }
    out
}

/// Each run of `f`, timed; prints the line and gives the median.
fn timed(name: &str, agents: usize, mut f: impl FnMut()) -> u128 {
    let mut took: Vec<u128> = (0..REPEATS)
        .map(|_| {
            let began = Instant::now();
            f();
            began.elapsed().as_micros()
        })
        .collect();
    took.sort_unstable();
    let p50 = took[took.len() / 2];
    println!("draw_bench {name} {agents} {p50} {}", took[took.len() - 1]);
    p50
}

#[test]
#[ignore]
fn draw_bench_projects() {
    for (count, per) in SIZES {
        let listed: Vec<projects::Project> = (0..count)
            .map(|p| {
                let rows = rows(p, per);
                let counts = status::counts(&rows);
                projects::Project {
                    lead: format!("p{p}.lead"),
                    name: format!("p{p}"),
                    title: format!("Proj {p:03}"),
                    folder: format!("/tmp/w/p{p}"),
                    counts,
                    state: "working".to_owned(),
                    last: NOW,
                    agents: status::agents(&format!("Proj {p:03}"), &rows),
                    ..Default::default()
                }
            })
            .collect();
        // The page set from a read and drawn, as an open does.
        timed("projects_open", count * per, || {
            let mut view = projects::View::new("p0.lead");
            view.set(listed.clone(), NOW);
            std::hint::black_box(view.lines(150, 40, NOW));
        });
        // The pick moved and the page drawn again.
        let mut view = projects::View::new("p0.lead");
        view.set(listed.clone(), NOW);
        let down =
            ratatui::crossterm::event::KeyEvent::from(ratatui::crossterm::event::KeyCode::Down);
        timed("projects_pick", count * per, || {
            view.key(&down, NOW);
            std::hint::black_box(view.lines(150, 40, NOW));
        });
    }
}

#[test]
#[ignore]
fn draw_bench_status() {
    for (count, per) in SIZES {
        let project = rows(0, per);
        timed("status_card", count * per, || {
            std::hint::black_box(status::card(
                "Proj 000",
                &project,
                45,
                &[],
                status::Details::Quiet,
                &[],
            ));
        });
        // Every project's rows counted, as the footer and Projects' head
        // count them.
        let every: Vec<Vec<status::Row>> = (0..count).map(|p| rows(p, per)).collect();
        timed("status_count_every", count * per, || {
            for rows in &every {
                std::hint::black_box(status::counts(rows));
            }
        });
    }
}

#[test]
#[ignore]
fn draw_bench_switch() {
    for (count, per) in SIZES {
        let bots = bots(count, per);
        let open = || {
            switch::Picker::new(
                &bots,
                |_| false,
                |_| None,
                "p0.lead",
                Some("p0"),
                "/tmp/w/p0",
                None,
                crate::fuzzy::Picks::default(),
            )
        };
        timed("switch_open", count * per, || {
            std::hint::black_box(open());
        });
        let mut picker = open();
        let typed = ["p", "p1", "p1.", "p1.t", "p1.t2"];
        timed("switch_type", count * per, || {
            for filter in typed {
                picker.narrow(filter.to_owned());
                std::hint::black_box(picker.rows().len());
            }
        });
    }
}
