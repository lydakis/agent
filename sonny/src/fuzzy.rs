//! How every list Sonny narrows by typing finds its rows: the slash menu,
//! `/model` and `sonny setup`'s pickers. A row matches when the letters
//! typed are in its name in order, the first of them starting the name or
//! a word in it, as VS Code's command palette matches: `/cms` finds
//! `/commons`, and `/co` does not find `/instructions`. Rows rank in tiers
//! as Codex's mention popup and Claude Code's command search rank them:
//! the whole name, then a name it starts, then a word in the name it
//! starts, then the letters. Within a tier, what the person picked more
//! often and more lately comes first, then the tighter match, then the
//! list's own order.
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// How well a name matches what was typed, best first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    Whole,
    Start,
    Word,
    Letters,
}

/// A name's match: its tier, how far apart its letters are, and where
/// they are, in the name's chars, for the list to show.
#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    pub tier: Tier,
    spread: usize,
    pub at: Vec<usize>,
}

/// Where a word starts in a name: after one of these, or at its start.
fn breaks(c: char) -> bool {
    matches!(c, '/' | '-' | '_' | '.' | ' ' | ':')
}

/// How `name` matches `typed`, ignoring case, if it does.
pub fn find(name: &str, typed: &str) -> Option<Found> {
    // Each lowered char keeps the place of the char it came from, as some
    // lower into more than one.
    let (lowered, from): (Vec<char>, Vec<usize>) = (name.chars().enumerate())
        .flat_map(|(at, c)| c.to_lowercase().map(move |l| (l, at)))
        .unzip();
    let typed: Vec<char> = typed.chars().flat_map(char::to_lowercase).collect();
    let places = |found: &[usize]| {
        let mut at: Vec<usize> = found.iter().map(|&i| from[i]).collect();
        at.dedup();
        at
    };
    if typed.is_empty() {
        return Some(Found {
            tier: Tier::Whole,
            spread: 0,
            at: Vec::new(),
        });
    }
    let starts = |at: usize| at == 0 || breaks(lowered[at - 1]);
    let word = (0..lowered.len()).find(|&at| starts(at) && lowered[at..].starts_with(&typed));
    if let Some(start) = word {
        let tier = match (start, lowered.len() == typed.len()) {
            (0, true) => Tier::Whole,
            (0, false) => Tier::Start,
            _ => Tier::Word,
        };
        let found: Vec<usize> = (start..start + typed.len()).collect();
        return Some(Found {
            tier,
            spread: 0,
            at: places(&found),
        });
    }
    // The tightest run of the letters in order: from each word the first
    // letter starts, the nearest of each next one. A long name, as a
    // prompt sent before can be, is not scanned on from every start: each
    // run keeps only where it starts and where it is, the runs end in
    // order so one pass finds every run's next letter, and runs that reach
    // the same letter go on alike, so only the later, tighter one is kept.
    // That costs the name's length times the letters typed, in time, and
    // its starts in memory; the winner's letters are found again at the end.
    let mut runs: Vec<(usize, usize)> = (0..lowered.len())
        .filter(|&at| starts(at) && lowered[at] == typed[0])
        .map(|at| (at, at))
        .collect();
    for &c in &typed[1..] {
        let mut seen = Some(0);
        runs.retain_mut(|(_, last)| {
            let from = *last + 1;
            if seen.is_some_and(|at| at < from) {
                seen = (lowered[from..].iter().position(|&l| l == c)).map(|at| from + at);
            }
            seen.map(|at| *last = at).is_some()
        });
        runs.dedup_by(|later, earlier| {
            let same = later.1 == earlier.1;
            if same {
                *earlier = *later;
            }
            same
        });
    }
    let (start, _) = runs.into_iter().min_by_key(|(start, last)| last - start)?;
    let mut found = vec![start];
    for &c in &typed[1..] {
        let from = found[found.len() - 1] + 1;
        found.push(from + lowered[from..].iter().position(|&l| l == c)?);
    }
    Some(Found {
        tier: Tier::Letters,
        spread: found[found.len() - 1] - found[0] + 1 - typed.len(),
        at: places(&found),
    })
}

/// The rows of `names` that match `typed`, best first, each with its place
/// among `names` and its match; `weight` is how much the person has picked
/// a row. With nothing typed, every row in the list's own order.
pub fn rank<'a>(
    names: impl IntoIterator<Item = &'a str>,
    typed: &str,
    weight: impl Fn(&str) -> f64,
) -> Vec<(usize, Found)> {
    let names: Vec<&str> = names.into_iter().collect();
    let found = names.iter().map(|name| find(name, typed));
    // Nothing typed keeps the list as it reads, so its places are learnt.
    order(found, |at| match typed.is_empty() {
        true => 0.0,
        false => weight(names[at]),
    })
}

/// The rows that matched, best first, from each row's match or none:
/// tier, then `weight`, then the tighter match, then the list's order.
pub fn order(
    found: impl IntoIterator<Item = Option<Found>>,
    weight: impl Fn(usize) -> f64,
) -> Vec<(usize, Found)> {
    let mut found: Vec<(usize, Found, f64)> = (found.into_iter().enumerate())
        .filter_map(|(at, found)| Some((at, found?, weight(at))))
        .collect();
    found.sort_by(|(a, fa, wa), (b, fb, wb)| {
        (fa.tier.cmp(&fb.tier))
            .then(wb.total_cmp(wa))
            .then(fa.spread.cmp(&fb.spread))
            .then(a.cmp(b))
    });
    found.into_iter().map(|(at, f, _)| (at, f)).collect()
}

impl Found {
    /// Whether the letters are side by side, as typed.
    pub fn together(&self) -> bool {
        self.spread == 0
    }
}

/// The better of two matches of one row, as a row matched by its label
/// or its detail.
pub fn better(a: Option<Found>, b: Option<Found>) -> Option<Found> {
    match (a, b) {
        (Some(a), Some(b)) => match (a.tier, a.spread).cmp(&(b.tier, b.spread)) {
            Ordering::Greater => Some(b),
            _ => Some(a),
        },
        (a, b) => a.or(b),
    }
}

/// A use's weight halves every week, and never falls below a tenth, as
/// Claude Code weighs the commands people run.
const HALF_LIFE_DAYS: f64 = 7.0;
const FLOOR: f64 = 0.1;

/// What the person picked from a list, and when last, kept in
/// `~/.agent/sonny/picks.json` so every window ranks the same.
#[derive(Default)]
pub struct Picks {
    file: Option<PathBuf>,
    list: String,
    /// By name: how often, and when last in milliseconds.
    on: BTreeMap<String, (u64, i64)>,
}

impl Picks {
    /// The person's picks from `list`, kept under `HOME`.
    pub fn load(list: &str) -> Self {
        match std::env::var_os("HOME").filter(|h| !h.is_empty()) {
            Some(home) => Self::kept(&PathBuf::from(home).join(".agent/sonny/picks.json"), list),
            None => Self::default(),
        }
    }

    pub fn kept(file: &Path, list: &str) -> Self {
        Self {
            on: read(file, list),
            file: Some(file.to_owned()),
            list: list.to_owned(),
        }
    }

    /// How much `name` was picked, at `now` in milliseconds.
    pub fn weight(&self, name: &str, now: i64) -> f64 {
        self.on.get(name).map_or(0.0, |&(times, last)| {
            let days = (now - last).max(0) as f64 / 86_400_000.0;
            times as f64 * 0.5_f64.powf(days / HALF_LIFE_DAYS).max(FLOOR)
        })
    }

    /// How much `name` was picked, now.
    pub fn weight_now(&self, name: &str) -> f64 {
        self.weight(name, now_ms())
    }

    /// Notes a pick of `name` now.
    pub fn pick_now(&mut self, name: &str) {
        self.pick(name, now_ms());
    }

    /// Notes a pick of `name` at `now`, read again first so another
    /// window's picks are kept.
    pub fn pick(&mut self, name: &str, now: i64) {
        if let Some(file) = &self.file {
            self.on = read(file, &self.list);
        }
        let (times, last) = self.on.entry(name.to_owned()).or_default();
        *times += 1;
        *last = now;
        let Some(file) = &self.file else {
            return;
        };
        let mut all: serde_json::Value = (std::fs::read(file).ok())
            .and_then(|text| serde_json::from_slice(&text).ok())
            .filter(serde_json::Value::is_object)
            .unwrap_or_else(|| serde_json::json!({}));
        all[&self.list] = (self.on.iter())
            .map(|(name, (times, last))| {
                (
                    name.clone(),
                    serde_json::json!({"times": times, "last": last}),
                )
            })
            .collect::<serde_json::Map<_, _>>()
            .into();
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let next = file.with_extension(format!("{}.next", std::process::id()));
        if std::fs::write(&next, format!("{all}\n")).is_ok() {
            let _ = std::fs::rename(&next, file);
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn read(file: &Path, list: &str) -> BTreeMap<String, (u64, i64)> {
    let all: serde_json::Value = (std::fs::read(file).ok())
        .and_then(|text| serde_json::from_slice(&text).ok())
        .unwrap_or_default();
    (all[list].as_object().into_iter().flatten())
        .filter_map(|(name, pick)| {
            Some((
                name.clone(),
                (pick["times"].as_u64()?, pick["last"].as_i64()?),
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(found: &[(usize, Found)], from: &[&'static str]) -> Vec<&'static str> {
        found.iter().map(|(at, _)| from[*at]).collect()
    }

    #[test]
    fn tiers_go_whole_start_word_letters() {
        assert_eq!(find("compact", "compact").unwrap().tier, Tier::Whole);
        assert_eq!(find("compact", "com").unwrap().tier, Tier::Start);
        assert_eq!(find("claude-opus-5-5", "opus").unwrap().tier, Tier::Word);
        let letters = find("commons", "cms").unwrap();
        assert_eq!(letters.tier, Tier::Letters);
        assert_eq!(letters.at, [0, 2, 6]);
        assert!(find("compact", "cms").is_none());
        assert!(
            find("instructions", "co").is_none(),
            "the first letter starts a word"
        );
        assert_eq!(find("claude-opus-5-5", "op55").unwrap().at, [7, 8, 12, 14]);
        assert!(find("Opus", "OP").is_some(), "case is ignored");
    }

    #[test]
    fn the_tightest_letters_win() {
        // The nearest of each letter after the first.
        assert_eq!(find("commons", "cos").unwrap().at, [0, 1, 6]);
        // Letters side by side beat the same letters apart.
        let ranked = rank(["b-g-r", "x-bgrx"], "bgr", |_| 0.0);
        assert_eq!(ranked[0].0, 1);
        assert!(ranked[0].1.together() && !ranked[1].1.together());
    }

    #[test]
    fn a_long_name_costs_its_length_not_its_square() {
        // A prompt sent before can be long, with a word start at every
        // other char: the letters are found at once from each, not by
        // scanning on from each, which took seconds here.
        let long = "a ".repeat(200_000) + "z";
        assert_eq!(find(&long, "az").unwrap().at, [399_998, 400_000]);
        assert!(find(&long, "zz").is_none());
        // A long query keeps one place per run, not one per letter.
        let long = "a ".repeat(5_000) + "z";
        let found = find(&long, &("a".repeat(500) + "z")).unwrap();
        assert_eq!(found.at.len(), 501);
        assert_eq!((found.at[0], found.at[500]), (9_000, 10_000));
    }

    #[test]
    fn picks_rank_within_a_tier_not_across() {
        let all = ["compact", "commons", "copy"];
        let none = rank(all, "com", |_| 0.0);
        assert_eq!(names(&none, &all), ["compact", "commons"]);
        let used = |name: &str| if name == "commons" { 3.0 } else { 0.0 };
        assert_eq!(names(&rank(all, "com", used), &all), ["commons", "compact"]);
        // The whole name still beats a row picked more.
        let copy = |name: &str| if name == "commons" { 9.0 } else { 0.0 };
        assert_eq!(names(&rank(all, "copy", copy), &all)[0], "copy");
        assert_eq!(
            names(&rank(all, "", copy), &all),
            all,
            "nothing typed keeps the order"
        );
    }

    #[test]
    fn a_pick_weighs_less_as_weeks_pass_and_is_kept_for_every_window() {
        let dir = std::env::temp_dir().join(format!("sonny-picks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("picks.json");
        let day = 86_400_000;
        let mut picks = Picks::kept(&file, "commands");
        picks.pick("/commons", 0);
        picks.pick("/commons", 0);
        assert_eq!(picks.weight("/commons", 0), 2.0);
        assert_eq!(picks.weight("/commons", 7 * day), 1.0);
        assert!((picks.weight("/commons", 365 * day) - 0.2).abs() < 1e-9);
        assert_eq!(picks.weight("/compact", 0), 0.0);
        // Another window's pick is read before this one writes.
        let mut other = Picks::kept(&file, "commands");
        other.pick("/sky", day);
        picks.pick("/commons", day);
        let again = Picks::kept(&file, "commands");
        assert_eq!(again.on["/commons"], (3, day));
        assert_eq!(again.on["/sky"], (1, day));
        assert!(
            Picks::kept(&file, "models").on.is_empty(),
            "one list's picks are its own"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
