//! Where a project's work is laid out: a terminal multiplexer the person
//! watches. Each project has a home there (a Rex session), with a window
//! per task and a block labelled with each bot's name, so it can be found
//! again by that name; a command a bot asked for is labelled `BOT/NAME` and
//! closes with its owner. `Layout` keeps what Sonny does there; a `Backend`
//! says how, in its multiplexer's CLI.
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

/// A block as its multiplexer lists it.
pub struct Block {
    pub id: String,
    pub label: String,
    pub window: String,
}

/// Where `sonny show` puts a task.
pub enum Placement {
    Window,
    /// Beside a block (a bot's, by its label), on one side.
    Split {
        side: String,
        beside: String,
    },
}

/// Where each block sits, by label: its window, counted from 1, and in a
/// split, its side of its neighbour (`right of`, `below`, … a label).
pub type Places = HashMap<String, (usize, Option<(String, String)>)>;

pub trait Backend {
    /// The project's home as the person reads it: `Rex session shop`.
    fn home(&self) -> String;
    /// Whether that home is the one Sonny runs in.
    fn here(&self) -> bool;
    /// Whether that home was made just now, with a shell nobody asked for.
    fn made(&self) -> bool;
    /// The home's blocks, in the multiplexer's order.
    fn blocks(&self) -> Vec<Block>;
    fn close_block(&self, block: &str);
    fn rename_block(&self, block: &str, label: &str);
    fn rename_window(&self, window: &str, name: &str);
    /// A new window named `name` whose one block, labelled `label`, runs
    /// `command`, leaving the person's focus where it is. `keep`: in `cwd`,
    /// and left open once the command exits so it can be read; otherwise
    /// it goes with the command. Returns the block's ID.
    fn window(
        &self,
        label: &str,
        name: &str,
        cwd: &Path,
        command: &[String],
        keep: bool,
    ) -> Result<String, String>;
    /// A new block beside `anchor`, on `side` (`right`, `left`, `above`,
    /// `below`), as `window` makes one; `small` makes it the smaller part.
    #[allow(clippy::too_many_arguments)]
    fn split(
        &self,
        anchor: &str,
        side: &str,
        small: bool,
        label: &str,
        cwd: &Path,
        command: &[String],
        keep: bool,
    ) -> Result<String, String>;
    /// The block Sonny itself runs in, when in this home.
    fn own_block(&self) -> Option<String>;
    fn places(&self) -> Places;
    /// The home, to find again.
    fn place(&self) -> Place;
}

/// A project's home, as its lead's Sonny last found it in the person's
/// terminal: kept so that an agent's `sonny run` and `sonny show` go
/// there, to that multiplexer, instead of one guessed from the agent's
/// shell, which is in neither.
#[derive(Debug, PartialEq)]
pub enum Place {
    Herdr {
        workspace: String,
    },
    Rex {
        session: String,
        host: Option<String>,
    },
}

impl Place {
    /// One file a project of a daemon, under `~/.agent/sonny/places`.
    fn file(socket: &Path, project: &str) -> Option<PathBuf> {
        let socket = std::fs::canonicalize(socket).unwrap_or_else(|_| socket.to_owned());
        let key = crate::project::fingerprint(&format!("{}\n{project}", socket.display()));
        let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
        Some(PathBuf::from(home).join(format!(".agent/sonny/places/{key}.json")))
    }

    fn keep(&self, path: &Path) {
        let json = match self {
            Self::Herdr { workspace } => {
                serde_json::json!({"multiplexer": "herdr", "workspace": workspace})
            }
            Self::Rex { session, host } => {
                serde_json::json!({"multiplexer": "rex", "session": session, "host": host})
            }
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let temp = path.with_extension(format!("{}.tmp", std::process::id()));
        if std::fs::write(&temp, format!("{json}\n")).is_ok() {
            let _ = std::fs::rename(&temp, path);
        }
    }

    fn recall(path: &Path) -> Option<Self> {
        let text = std::fs::read(path).ok()?;
        let json: serde_json::Value = serde_json::from_slice(&text).ok()?;
        let text = |key: &str| json[key].as_str().map(str::to_owned);
        match json["multiplexer"].as_str()? {
            "herdr" => Some(Self::Herdr {
                workspace: text("workspace")?,
            }),
            "rex" => Some(Self::Rex {
                session: text("session")?,
                host: text("host"),
            }),
            _ => None,
        }
    }
}

pub struct Layout {
    backend: Box<dyn Backend>,
    /// Each task's block, by bot name.
    blocks: HashMap<String, String>,
    /// The blocks a home made just now came with, closed once something
    /// is placed in it.
    spare: Vec<String>,
}

impl Layout {
    pub fn new(backend: Box<dyn Backend>) -> Self {
        let spare = match backend.made() {
            true => backend.blocks().into_iter().map(|b| b.id).collect(),
            false => Vec::new(),
        };
        Self {
            backend,
            blocks: HashMap::new(),
            spare,
        }
    }

    /// Close the shell a home made just now came with, now that it holds
    /// something of its own.
    fn tidy(&mut self) {
        for block in std::mem::take(&mut self.spare) {
            self.backend.close_block(&block);
        }
    }

    pub fn home(&self) -> String {
        self.backend.home()
    }

    pub fn here(&self) -> bool {
        self.backend.here()
    }

    /// A block already labelled with the bot's name.
    fn adopt(&mut self, bot: &str, listed: &[Block]) -> bool {
        let Some(found) = listed.iter().find(|b| b.label == bot) else {
            return false;
        };
        self.blocks.insert(bot.to_owned(), found.id.clone());
        true
    }

    /// A window for the task running `command`, named `name`, unless the
    /// task has a block already. Returns whether one was opened. The
    /// person's focus stays where it is.
    pub fn show(&mut self, bot: &str, name: &str, command: &[String]) -> Result<bool, String> {
        let listed = self.backend.blocks();
        if let Some(block) = self.blocks.get(bot)
            && listed.iter().any(|b| &b.id == block)
        {
            return Ok(false);
        }
        if self.adopt(bot, &listed) {
            return Ok(false);
        }
        self.place(bot, name, command, &Placement::Window)?;
        Ok(true)
    }

    /// Open the task's block where asked, closing any it had.
    pub fn place(
        &mut self,
        bot: &str,
        name: &str,
        command: &[String],
        placement: &Placement,
    ) -> Result<(), String> {
        let listed = self.backend.blocks();
        for old in listed.iter().filter(|b| b.label == bot) {
            self.backend.close_block(&old.id);
        }
        let cwd = Path::new("/");
        let block = match placement {
            Placement::Window => self.backend.window(bot, name, cwd, command, false)?,
            Placement::Split { side, beside } => {
                let anchor = listed
                    .iter()
                    .find(|b| &b.label == beside && b.label != bot)
                    .map(|b| b.id.clone())
                    .ok_or(format!("no block labelled {beside} in {}", self.home()))?;
                (self.backend).split(&anchor, side, false, bot, cwd, command, false)?
            }
        };
        self.blocks.insert(bot.to_owned(), block);
        self.tidy();
        Ok(())
    }

    /// Each window and the labels of its blocks, in the multiplexer's order.
    pub fn windows(&self) -> Vec<(String, Vec<String>)> {
        let mut windows: Vec<(String, Vec<String>)> = Vec::new();
        for block in self.backend.blocks() {
            match windows
                .iter_mut()
                .find(|(window, _)| *window == block.window)
            {
                Some((_, labels)) => labels.push(block.label),
                None => windows.push((block.window, vec![block.label])),
            }
        }
        windows
    }

    pub fn places(&self) -> Places {
        self.backend.places()
    }

    pub fn rename_window(&self, window: &str, name: &str) {
        self.backend.rename_window(window, name);
    }

    /// Take on the block Sonny itself runs in, when in this home, named
    /// after the bot.
    pub fn adopt_own(&mut self, bot: &str) {
        let mine = self.backend.own_block();
        let listed = self.backend.blocks();
        let own = listed
            .iter()
            .find(|b| mine.as_ref().is_some_and(|mine| &b.id == mine))
            .or_else(|| listed.iter().find(|b| b.label == bot));
        let Some(own) = own else {
            return;
        };
        if own.label != bot {
            self.backend.rename_block(&own.id, bot);
        }
        self.blocks.insert(bot.to_owned(), own.id.clone());
    }

    /// Close every other block labelled with the bot's name, as one Sonny
    /// opened for the task while this one placed it.
    pub fn close_others(&mut self, bot: &str) {
        let Some(mine) = self.blocks.get(bot).cloned() else {
            return;
        };
        for other in self
            .backend
            .blocks()
            .into_iter()
            .filter(|b| b.label == bot && b.id != mine)
        {
            self.backend.close_block(&other.id);
        }
    }

    /// Run a command an agent asked for in a block labelled `OWNER/NAME`, so
    /// the person can see whose it is and it closes with its owner's
    /// window: on `side` of the owner's block, or in a window of its own
    /// (named `name`) when there is no side or the owner has no block here.
    /// One already labelled so is replaced. The block stays open once the
    /// command exits, to be read. Returns the block's ID.
    pub fn split(
        &mut self,
        owner: &str,
        name: &str,
        cwd: &Path,
        command: &[String],
        side: Option<&str>,
    ) -> Result<String, String> {
        let label = format!("{owner}/{name}");
        let listed = self.backend.blocks();
        for old in listed.iter().filter(|b| b.label == label) {
            self.backend.close_block(&old.id);
        }
        let anchor = listed.iter().find(|b| b.label == owner);
        let block = match anchor.zip(side) {
            Some((anchor, side)) => {
                (self.backend).split(&anchor.id, side, true, &label, cwd, command, true)
            }
            None => self.backend.window(&label, name, cwd, command, true),
        }?;
        self.tidy();
        Ok(block)
    }

    /// Close a task's block, and every block it asked for, as when it is
    /// done or its bot is deleted.
    pub fn close(&mut self, bot: &str) {
        let known = self.blocks.remove(bot);
        let owned = format!("{bot}/");
        for block in self.backend.blocks().into_iter().filter(|b| {
            b.label == bot || b.label.starts_with(&owned) || Some(&b.id) == known.as_ref()
        }) {
            self.backend.close_block(&block.id);
        }
    }
}

/// The multiplexers Sonny lays work out in, by the CLI that drives each.
enum Kind {
    Rex(PathBuf),
    Herdr(PathBuf),
}

/// The multiplexer this terminal is in: Herdr when Sonny runs in a Herdr
/// pane, Rex when in a Rex block or `host` names a Rex server.
fn current(host: Option<&str>) -> Option<Kind> {
    if host.is_none() && crate::herdr::pane().is_some() {
        return crate::herdr::find().map(Kind::Herdr);
    }
    let rex = std::env::var_os("REX_SESSION").is_some() || host.is_some();
    rex.then(crate::rex::find).flatten().map(Kind::Rex)
}

/// The project's home in the multiplexer Sonny runs in, from the person's
/// terminal, kept as the place its agents' work goes. None when Sonny runs
/// in none; tasks then run out of sight, in /status.
pub fn here(socket: &Path, project: &str, host: Option<&str>) -> Option<Layout> {
    let backend: Box<dyn Backend> = match current(host)? {
        Kind::Rex(bin) => Box::new(crate::rex::Rex::open(bin, host, project, true, None).ok()?),
        Kind::Herdr(bin) => {
            let pane = crate::herdr::pane();
            Box::new(crate::herdr::Herdr::open(bin, project, pane.as_deref(), None).ok()?)
        }
    };
    if let Some(file) = Place::file(socket, project) {
        backend.place().keep(&file);
    }
    Some(Layout::new(backend))
}

/// The project's home, from an agent's shell, whose multiplexer variables
/// would name the daemon's terminal: where the lead's Sonny last laid the
/// project out, in that multiplexer; without one (or with `host`, a Rex
/// server), in Herdr when the project's lead is in a Herdr pane, else in
/// the multiplexer project `like`'s home is in, else in Rex (or Herdr,
/// without Rex), made when needed.
pub fn for_agent(
    socket: &Path,
    project: &str,
    host: Option<&str>,
    like: Option<&str>,
) -> Result<Layout, String> {
    let herdr = crate::herdr::find().filter(|_| host.is_none());
    let rex = crate::rex::find();
    let recall = |project| {
        Place::file(socket, project)
            .filter(|_| host.is_none())
            .and_then(|file| Place::recall(&file))
    };
    let kept = recall(project);
    let like = like.and_then(recall);
    let backend: Box<dyn Backend> = match (kept, herdr, rex) {
        (Some(Place::Herdr { workspace }), Some(herdr), _) => Box::new(crate::herdr::Herdr::open(
            herdr,
            project,
            None,
            Some(&workspace),
        )?),
        (Some(Place::Rex { session, host }), _, Some(rex)) => Box::new(crate::rex::Rex::open(
            rex,
            host.as_deref(),
            project,
            false,
            Some(&session),
        )?),
        (_, Some(herdr), _)
            if crate::herdr::has_lead(&herdr, project)
                || matches!(like, Some(Place::Herdr { .. })) =>
        {
            Box::new(crate::herdr::Herdr::open(herdr, project, None, None)?)
        }
        (_, _, Some(rex)) if let Some(Place::Rex { host, .. }) = &like => Box::new(
            crate::rex::Rex::open(rex, host.as_deref(), project, false, None)?,
        ),
        (_, _, Some(rex)) => Box::new(crate::rex::Rex::open(rex, host, project, false, None)?),
        (_, Some(herdr), None) => Box::new(crate::herdr::Herdr::open(herdr, project, None, None)?),
        (_, None, None) => return Err("neither rex nor herdr was found".into()),
    };
    Ok(Layout::new(backend))
}

/// The skills that let any agent run long work where the person can see
/// it, for the multiplexer this terminal is in (the other's taken out, so
/// agents read one), and show the person pictures. Each names this
/// machine's multiplexer CLI and Sonny.
pub fn install_skill(host: Option<&str>) {
    crate::skills::install("pictures", include_str!("../skills/pictures/SKILL.md"));
    let Some(sonny) = std::env::current_exe().ok() else {
        return;
    };
    let quoted = |path: &Path| format!("\"{}\"", path.to_string_lossy());
    let rex = |bin: PathBuf| {
        let text = include_str!("../skills/rex-windows/SKILL.md");
        (
            "rex-windows",
            "herdr-panes",
            text.replace("REX_BIN", &quoted(&bin)),
        )
    };
    let (name, other, text) = match current(host) {
        Some(Kind::Herdr(bin)) => {
            let text = include_str!("../skills/herdr-panes/SKILL.md");
            (
                "herdr-panes",
                "rex-windows",
                text.replace("HERDR_BIN", &quoted(&bin)),
            )
        }
        Some(Kind::Rex(bin)) => rex(bin),
        // Out of sight, Rex is the default.
        None => match crate::rex::find() {
            Some(bin) => rex(bin),
            None => return,
        },
    };
    crate::skills::install(name, &text.replace("SONNY_BIN", &quoted(&sonny)));
    crate::skills::remove(other);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A multiplexer in memory: its blocks, and Sonny's own.
    #[derive(Default)]
    struct Fake {
        blocks: RefCell<Vec<Block>>,
        own: Option<String>,
        made: bool,
    }

    impl Fake {
        fn add(&self, label: &str, window: &str) -> String {
            let id = format!("b{}", self.blocks.borrow().len() + 1);
            self.blocks.borrow_mut().push(Block {
                id: id.clone(),
                label: label.into(),
                window: window.into(),
            });
            id
        }
    }

    impl Backend for Fake {
        fn home(&self) -> String {
            "Fake home".into()
        }
        fn here(&self) -> bool {
            true
        }
        fn made(&self) -> bool {
            self.made
        }
        fn blocks(&self) -> Vec<Block> {
            (self.blocks.borrow().iter())
                .map(|b| Block {
                    id: b.id.clone(),
                    label: b.label.clone(),
                    window: b.window.clone(),
                })
                .collect()
        }
        fn close_block(&self, block: &str) {
            self.blocks.borrow_mut().retain(|b| b.id != block);
        }
        fn rename_block(&self, block: &str, label: &str) {
            for b in self
                .blocks
                .borrow_mut()
                .iter_mut()
                .filter(|b| b.id == block)
            {
                b.label = label.into();
            }
        }
        fn rename_window(&self, _: &str, _: &str) {}
        fn window(
            &self,
            label: &str,
            name: &str,
            _: &Path,
            _: &[String],
            _: bool,
        ) -> Result<String, String> {
            Ok(self.add(label, name))
        }
        fn split(
            &self,
            anchor: &str,
            _: &str,
            _: bool,
            label: &str,
            _: &Path,
            _: &[String],
            _: bool,
        ) -> Result<String, String> {
            let window = (self.blocks.borrow().iter())
                .find(|b| b.id == anchor)
                .map(|b| b.window.clone())
                .ok_or("no anchor")?;
            Ok(self.add(label, &window))
        }
        fn own_block(&self) -> Option<String> {
            self.own.clone()
        }
        fn places(&self) -> Places {
            Places::new()
        }
        fn place(&self) -> Place {
            Place::Herdr {
                workspace: "w1".into(),
            }
        }
    }

    #[test]
    fn a_project_keeps_the_place_it_was_laid_out_in() {
        let file = std::env::temp_dir().join(format!("sonny-place-{}.json", std::process::id()));
        let herdr = Place::Herdr {
            workspace: "wD".into(),
        };
        herdr.keep(&file);
        assert_eq!(Place::recall(&file), Some(herdr));
        let rex = Place::Rex {
            session: "s7".into(),
            host: Some("mini".into()),
        };
        rex.keep(&file);
        assert_eq!(Place::recall(&file), Some(rex));
        std::fs::write(&file, "{\"multiplexer\": \"tmux\"}").unwrap();
        assert_eq!(Place::recall(&file), None);
        let _ = std::fs::remove_file(&file);
        let socket = Path::new("/tmp/agent.sock");
        assert_ne!(Place::file(socket, "shop"), Place::file(socket, "shop.web"));
    }

    fn labels(layout: &Layout) -> Vec<(String, Vec<String>)> {
        layout.windows()
    }

    #[test]
    fn a_task_has_one_block_wherever_it_is_put_and_its_commands_close_with_it() {
        let fake = Fake::default();
        let own = fake.add("zsh", "home");
        let fake = Fake {
            own: Some(own),
            ..fake
        };
        let mut layout = Layout::new(Box::new(fake));
        layout.adopt_own("shop.lead");
        let sonny = ["sonny".to_owned()];
        assert!(layout.show("shop.fix", "fix", &sonny).unwrap());
        // Shown again, it is already there.
        assert!(!layout.show("shop.fix", "fix", &sonny).unwrap());
        let split = Placement::Split {
            side: "right".into(),
            beside: "shop.lead".into(),
        };
        layout.place("shop.fix", "fix", &sonny, &split).unwrap();
        layout
            .split("shop.fix", "tests", Path::new("/w"), &sonny, Some("below"))
            .unwrap();
        layout
            .split("shop.lead", "server", Path::new("/w"), &sonny, None)
            .unwrap();
        assert_eq!(
            labels(&layout),
            [
                (
                    "home".into(),
                    vec![
                        "shop.lead".into(),
                        "shop.fix".into(),
                        "shop.fix/tests".into()
                    ]
                ),
                ("server".into(), vec!["shop.lead/server".into()]),
            ]
        );
        layout.close("shop.fix");
        assert_eq!(
            labels(&layout),
            [
                ("home".into(), vec!["shop.lead".into()]),
                ("server".into(), vec!["shop.lead/server".into()]),
            ]
        );
    }

    #[test]
    fn a_home_made_for_a_lead_holds_only_its_sonny() {
        let fake = Fake {
            made: true,
            ..Fake::default()
        };
        fake.add("", "shell");
        let mut layout = Layout::new(Box::new(fake));
        let sonny = ["sonny".to_owned()];
        assert!(layout.show("web.lead", "lead", &sonny).unwrap());
        assert_eq!(labels(&layout), [("lead".into(), vec!["web.lead".into()])]);
        // The lead's Sonny is there already: left alone.
        assert!(!layout.show("web.lead", "lead", &sonny).unwrap());
    }

    #[test]
    fn a_home_sonny_was_opened_in_keeps_what_was_there() {
        let fake = Fake::default();
        fake.add("editor", "work");
        let own = fake.add("zsh", "work");
        let fake = Fake {
            own: Some(own),
            ..fake
        };
        let mut layout = Layout::new(Box::new(fake));
        layout.adopt_own("shop.lead");
        let sonny = ["sonny".to_owned()];
        layout.show("shop.fix", "fix", &sonny).unwrap();
        layout
            .split(
                "shop.lead",
                "server",
                Path::new("/w"),
                &sonny,
                Some("right"),
            )
            .unwrap();
        layout.close("shop.fix");
        assert_eq!(
            labels(&layout),
            [(
                "work".into(),
                vec![
                    "editor".into(),
                    "shop.lead".into(),
                    "shop.lead/server".into()
                ]
            )]
        );
    }
}
