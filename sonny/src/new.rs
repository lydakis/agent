//! `/new [NAME]`: an agent started here, in three steps over the composer.
//! First its profile, from every profile file the folder and the person
//! have and the ones Sonny ships, the roles of a lead first and the
//! project's own marked; then, for an agent that is not a lead, where it
//! works: a worktree of its own, this folder, or an empty folder; then the
//! first message, which starts it and, without NAME, names it. Nothing is
//! made before that message is sent, and Esc goes back a step. A lead stays
//! in this folder, as every lead of a folder does; another agent is this
//! lead's task, as one it started would be. Codex asks where a new thread
//! works when it starts and Claude Code's agents take `isolation`, so the
//! place is the person's pick each time, not a profile's.
use crate::list::{Act, Picker};
use std::path::Path;

/// A profile to start an agent in.
#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    pub name: String,
    /// Its `description`, or its first line.
    pub about: String,
    /// A lead's role: `lead`, one built on it, or the project's own.
    pub lead: bool,
    /// The role this project's leads run as.
    pub default: bool,
}

/// Where an agent that is not a lead works.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Place {
    /// A worktree on a branch of its own, as a lead's tasks start.
    Worktree,
    /// This folder itself, beside the lead.
    Here,
    /// An empty folder of its own.
    Empty,
}

impl Place {
    pub fn label(self) -> &'static str {
        match self {
            Place::Worktree => "worktree",
            Place::Here => "this folder",
            Place::Empty => "empty folder",
        }
    }

    fn about(self) -> &'static str {
        match self {
            Place::Worktree => "a branch and a copy of the files of its own",
            Place::Here => "works on these files beside you",
            Place::Empty => "a new folder, for work that starts from nothing",
        }
    }

    /// As the heading over the first message says it.
    fn said(self) -> &'static str {
        match self {
            Place::Worktree => "in a worktree",
            Place::Here => "in this folder",
            Place::Empty => "in an empty folder",
        }
    }
}

/// The places an agent can work from `dir`: a worktree only in a git
/// repository, where it comes first; only this folder in a project whose
/// tasks all work in it.
pub fn places(dir: &Path) -> Vec<Place> {
    let info = agent_client::project::read(dir).unwrap_or_default();
    if info["threads_in"] == "project" {
        return vec![Place::Here];
    }
    match agent_client::policy::checkout(dir) {
        Some(_) => vec![Place::Worktree, Place::Here, Place::Empty],
        None => vec![Place::Here, Place::Empty],
    }
}

/// Every profile an agent can start in from `dir`: the folder's files,
/// then the person's, then the ones Sonny ships, each name once. The
/// roles of a lead come first, `default` first among them, then the rest,
/// each by name.
pub fn profiles(dir: &Path, default: &str) -> Result<Vec<Profile>, String> {
    let files = agent_client::policy::profiles(dir).map_err(|e| format!("{}: {e}", e.code()))?;
    let mut all: Vec<(String, String)> = (files.into_iter())
        .map(|entry| (entry.name, entry.summary))
        .collect();
    for (name, text) in crate::project::BUILTINS {
        if !all.iter().any(|(have, _)| have == name) {
            let (front, _) = agent_client::policy::front_matter(text);
            all.push((name.to_owned(), front.description.unwrap_or_default()));
        }
    }
    // A lead's role is `lead` or one built on it, however far down, as the
    // role is put together when the agent starts.
    let leads = |name: &str| {
        agent_client::policy::lineage(dir, name, &crate::project::BUILTINS)
            .is_ok_and(|built| built.iter().any(|base| base == "lead"))
    };
    let mut listed: Vec<Profile> = (all.iter())
        .map(|(name, about)| Profile {
            name: name.clone(),
            about: about.clone(),
            lead: name == default || leads(name),
            default: name == default,
        })
        .collect();
    listed.sort_by(|a, b| (!a.lead, !a.default, &a.name).cmp(&(!b.lead, !b.default, &b.name)));
    Ok(listed)
}

/// The step `/new` is at.
pub enum Step {
    /// Picking the profile.
    Profile(Picker),
    /// Picking where an agent that is not a lead works.
    Place(Picker),
    /// Writing the first message, in the composer.
    Brief,
}

/// What a key did to `/new`.
#[derive(Debug, PartialEq)]
pub enum Done {
    /// It stays open.
    Stay,
    /// It closed, nothing made.
    Close,
}

/// `/new`, open over the composer.
pub struct Making {
    /// The name given after `/new`, else the first message names it.
    pub name: Option<String>,
    pub profiles: Vec<Profile>,
    places: Vec<Place>,
    pub step: Step,
    profile: usize,
    place: Option<Place>,
    /// Why the last send started nothing, said over the message kept to
    /// try again, where a busy lead's status line would hide it.
    pub failed: Option<String>,
}

impl Making {
    pub fn new(name: Option<String>, profiles: Vec<Profile>, places: Vec<Place>) -> Self {
        let step = Step::Profile(profile_list(&profiles, 0));
        Self {
            name,
            profiles,
            places,
            step,
            profile: 0,
            place: None,
            failed: None,
        }
    }

    /// The profile and place picked: a lead has no place.
    pub fn chosen(&self) -> (&Profile, Option<Place>) {
        (&self.profiles[self.profile], self.place)
    }

    /// What a key does while a list is open: Enter takes its pick and goes
    /// on, Esc goes back a step, or closes from the first.
    pub fn event(&mut self, event: &ratatui::crossterm::event::Event) -> Done {
        match &mut self.step {
            Step::Profile(list) => match list.event(event) {
                Act::Close => return Done::Close,
                Act::Take(at) => {
                    self.profile = at;
                    let lead = self.profiles[at].lead;
                    // One place to work in is no question.
                    self.place = (!lead).then(|| self.places[0]);
                    self.step = match lead || self.places.len() < 2 {
                        true => Step::Brief,
                        false => Step::Place(place_list(&self.places, 0)),
                    };
                }
                Act::Stay | Act::Other => {}
            },
            Step::Place(list) => match list.event(event) {
                Act::Close => self.step = Step::Profile(profile_list(&self.profiles, self.profile)),
                Act::Take(at) => {
                    self.place = Some(self.places[at]);
                    self.step = Step::Brief;
                }
                Act::Stay | Act::Other => {}
            },
            Step::Brief => {}
        }
        Done::Stay
    }

    /// Back a step from the first message: to where it works, if that was
    /// asked, else to the profile.
    pub fn back(&mut self) {
        let asked = !self.profiles[self.profile].lead && self.places.len() > 1;
        self.step = match (asked, self.place) {
            (true, Some(place)) => {
                let at = self.places.iter().position(|p| *p == place).unwrap_or(0);
                Step::Place(place_list(&self.places, at))
            }
            _ => Step::Profile(profile_list(&self.profiles, self.profile)),
        };
    }

    /// The muted line over the composer while the first message is
    /// written: what Enter starts, and what names it.
    pub fn heading(&self) -> String {
        let (profile, place) = self.chosen();
        // A lead stays in this folder, as every lead of it does.
        let place = place.map_or("in this folder", Place::said);
        let named = match &self.name {
            Some(name) => format!("named {name}"),
            None => "named from your words".to_owned(),
        };
        format!("new {} {place} · {named}", profile.name)
    }

    /// The open list's rows as drawn, and its picker: the profiles with the
    /// project's own marked, or the places; none while the message is
    /// written.
    pub fn listed(&self) -> Option<(Vec<crate::list::Item>, &Picker)> {
        use crate::list::Item;
        match &self.step {
            Step::Profile(list) => {
                let items = (list.shown().into_iter())
                    .map(|(at, found)| {
                        let profile = &self.profiles[at];
                        Item::new(profile.name.clone(), profile.about.clone())
                            .current(profile.default)
                            .found(&found)
                    })
                    .collect();
                Some((items, list))
            }
            Step::Place(list) => {
                let items = (list.shown().into_iter())
                    .map(|(at, found)| {
                        let place = self.places[at];
                        Item::new(place.label(), place.about()).found(&found)
                    })
                    .collect();
                Some((items, list))
            }
            Step::Brief => None,
        }
    }

    /// What Esc does in the open list, for its key line.
    pub fn keys(&self) -> &'static str {
        match self.step {
            Step::Profile(_) => "  ↑↓ pick · enter next · esc close",
            _ => "  ↑↓ pick · enter next · esc back",
        }
    }

    /// What the list open asks, muted in the composer until typed over.
    pub fn asked(&self) -> String {
        match &self.step {
            Step::Profile(_) => "new agent: which profile? type to narrow".into(),
            Step::Place(_) => format!(
                "new {}: where does it work?",
                self.profiles[self.profile].name
            ),
            Step::Brief => "what should it do?".into(),
        }
    }
}

fn profile_list(profiles: &[Profile], pick: usize) -> Picker {
    let rows = (profiles.iter())
        .map(|profile| (profile.name.clone(), profile.about.clone()))
        .collect();
    Picker::new(rows, pick)
}

fn place_list(places: &[Place], pick: usize) -> Picker {
    let rows = (places.iter())
        .map(|place| (place.label().to_owned(), place.about().to_owned()))
        .collect();
    Picker::new(rows, pick)
}

/// The name a worker `/new` starts takes from `words`, its first message,
/// as a lead's is: the first free of `taken` from it, numbered from 2.
pub fn task_name(words: &str, taken: impl Fn(&str) -> bool) -> String {
    let stem = match crate::project::lead_name_from(words) {
        // A task's name is never a lead's.
        stem if stem == "lead" || stem.starts_with("lead-") => format!("task-{stem}"),
        stem => stem,
    };
    let stem: String = stem.chars().take(28).collect();
    let stem = stem.trim_end_matches('-').to_owned();
    if !taken(&stem) {
        return stem;
    }
    (2..)
        .map(|n| format!("{stem}-{n}"))
        .find(|name| !taken(name))
        .unwrap_or(stem)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{Event, KeyCode};

    fn key(code: KeyCode) -> Event {
        Event::Key(code.into())
    }

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sonny-new-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".agents/agents")).unwrap();
        dir
    }

    fn profile(name: &str, lead: bool) -> Profile {
        Profile {
            name: name.into(),
            about: format!("{name} does"),
            lead,
            default: name == "lead",
        }
    }

    #[test]
    fn every_profile_is_listed_the_leads_first_the_projects_marked() {
        let dir = temp("profiles");
        let agents = dir.join(".agents/agents");
        std::fs::write(
            agents.join("reviewer.md"),
            "---\ndescription: Reviews a change\n---\nYou review.\n",
        )
        .unwrap();
        std::fs::write(
            agents.join("my-lead.md"),
            "---\nextends: superlead\ndescription: Mine\n---\nMore.\n",
        )
        .unwrap();
        std::fs::write(
            agents.join("careful.md"),
            "---\nextends: task\n---\nSlowly.\n",
        )
        .unwrap();
        // Built on the shipped superlead it shadows, so a lead's still.
        std::fs::write(
            agents.join("superlead.md"),
            "---\nextends: superlead\n---\nAnd this.\n",
        )
        .unwrap();
        let listed = profiles(&dir, "lead").unwrap();
        let names: Vec<(&str, bool)> = (listed.iter())
            .filter(|p| !p.name.starts_with('_'))
            .map(|p| (p.name.as_str(), p.lead))
            .collect();
        // The person's own files under HOME may add more; these are here.
        for want in [
            ("lead", true),
            ("my-lead", true),
            ("superlead", true),
            ("careful", false),
            ("reviewer", false),
            ("task", false),
        ] {
            assert!(names.contains(&want), "{want:?} in {names:?}");
        }
        assert_eq!(listed[0].name, "lead");
        assert!(listed[0].default);
        let first_worker = listed.iter().position(|p| !p.lead).unwrap();
        assert!(listed[first_worker..].iter().all(|p| !p.lead));
        let reviewer = listed.iter().find(|p| p.name == "reviewer").unwrap();
        assert_eq!(reviewer.about, "Reviews a change");
        // A project set to its own role marks it, a lead's whatever it is.
        let listed = profiles(&dir, "reviewer").unwrap();
        assert_eq!(listed[0].name, "reviewer");
        assert!(listed[0].lead && listed[0].default);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_worker_is_asked_where_it_works_and_a_lead_is_not() {
        let profiles = vec![profile("lead", true), profile("reviewer", false)];
        let places = vec![Place::Worktree, Place::Here, Place::Empty];
        let mut making = Making::new(None, profiles.clone(), places.clone());
        // A lead goes straight to its first message.
        making.event(&key(KeyCode::Enter));
        assert!(matches!(making.step, Step::Brief));
        assert_eq!(making.chosen(), (&profiles[0], None));
        assert_eq!(
            making.heading(),
            "new lead in this folder · named from your words"
        );
        // Back is the profile again.
        making.back();
        assert!(matches!(making.step, Step::Profile(_)));
        making.event(&key(KeyCode::Down));
        making.event(&key(KeyCode::Enter));
        assert!(matches!(making.step, Step::Place(_)));
        assert_eq!(making.asked(), "new reviewer: where does it work?");
        making.event(&key(KeyCode::Down));
        making.event(&key(KeyCode::Enter));
        assert_eq!(making.chosen(), (&profiles[1], Some(Place::Here)));
        assert_eq!(
            making.heading(),
            "new reviewer in this folder · named from your words"
        );
        // Back from the message is where it works, the pick kept; Esc
        // there is the profile, and Esc then closes.
        making.back();
        match &making.step {
            Step::Place(list) => assert_eq!(list.chosen(), Some(1)),
            _ => panic!("back to where it works"),
        }
        assert_eq!(making.event(&key(KeyCode::Esc)), Done::Stay);
        assert!(matches!(making.step, Step::Profile(_)));
        assert_eq!(making.event(&key(KeyCode::Esc)), Done::Close);
        // Typing narrows the profiles; one place is no question.
        let mut making = Making::new(Some("check".into()), profiles.clone(), vec![Place::Here]);
        for c in "rev".chars() {
            making.event(&key(KeyCode::Char(c)));
        }
        making.event(&key(KeyCode::Enter));
        assert!(matches!(making.step, Step::Brief));
        assert_eq!(
            making.heading(),
            "new reviewer in this folder · named check"
        );
    }

    #[test]
    fn a_folder_outside_git_has_no_worktree_and_a_shared_project_one_place() {
        let dir = temp("places");
        assert_eq!(places(&dir), [Place::Here, Place::Empty]);
        std::fs::write(
            dir.join(agent_client::project::FILE),
            "name = \"p\"\nthreads_in = \"project\"\n",
        )
        .unwrap();
        assert_eq!(places(&dir), [Place::Here]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_workers_name_comes_from_its_words_and_is_free() {
        let taken = |name: &str| name == "fix-the-login-redirect";
        assert_eq!(
            task_name("Fix the login redirect, it loops", taken),
            "fix-the-login-redirect-2"
        );
        assert_eq!(task_name("Check the docs", taken), "check-the-docs");
        assert_eq!(task_name("lead the way", |_| false), "task-lead-the-way");
    }
}
