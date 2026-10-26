//! Where a project's work is laid out: a terminal multiplexer the person
//! watches. Each project has a home there (a Rex session), with a window
//! per task and a block labelled with each bot's name, so it can be found
//! again by that name; a command a bot asked for is labelled `BOT/NAME` and
//! closes with its owner. `Layout` keeps what Sonny does there; a `Backend`
//! says how, in its multiplexer's CLI.
use std::{collections::HashMap, path::Path};

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
}

pub struct Layout {
    backend: Box<dyn Backend>,
    /// Each task's block, by bot name.
    blocks: HashMap<String, String>,
}

impl Layout {
    pub fn new(backend: Box<dyn Backend>) -> Self {
        Self {
            backend,
            blocks: HashMap::new(),
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
    /// window: below the owner's block, or in a window of its own (named
    /// `name`) when asked or when the owner has none here. One already
    /// labelled so is replaced. The block stays open once the command
    /// exits, to be read. Returns the block's ID.
    pub fn split(
        &mut self,
        owner: &str,
        name: &str,
        cwd: &Path,
        command: &[String],
        window: bool,
    ) -> Result<String, String> {
        let label = format!("{owner}/{name}");
        let listed = self.backend.blocks();
        for old in listed.iter().filter(|b| b.label == label) {
            self.backend.close_block(&old.id);
        }
        let anchor = listed
            .iter()
            .find(|b| b.label == owner)
            .filter(|_| !window)
            .map(|b| b.id.clone());
        match anchor {
            Some(anchor) => {
                (self.backend).split(&anchor, "below", true, &label, cwd, command, true)
            }
            None => self.backend.window(&label, name, cwd, command, true),
        }
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

/// The project's home in the multiplexer Sonny runs in, from the person's
/// terminal: Rex when Sonny runs in a Rex block, or `host` names a Rex
/// server. None when Sonny runs in neither; tasks then run out of sight,
/// in /status.
pub fn here(project: &str, host: Option<&str>) -> Option<Layout> {
    if std::env::var_os("REX_SESSION").is_none() && host.is_none() {
        return None;
    }
    let rex = crate::rex::Rex::open(crate::rex::find()?, host, project, true).ok()?;
    Some(Layout::new(Box::new(rex)))
}

/// The project's home, from an agent's shell, whose multiplexer variables
/// would name the daemon's terminal.
pub fn for_agent(project: &str, host: Option<&str>) -> Result<Layout, String> {
    let bin = crate::rex::find().ok_or("rex not found")?;
    let rex = crate::rex::Rex::open(bin, host, project, false)?;
    Ok(Layout::new(Box::new(rex)))
}

/// The skill that tells agents how to use the multiplexer.
pub fn install_skill() {
    if let Some(bin) = crate::rex::find() {
        crate::rex::install_skill(&bin);
    }
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
            .split("shop.fix", "tests", Path::new("/w"), &sonny, false)
            .unwrap();
        layout
            .split("shop.lead", "server", Path::new("/w"), &sonny, true)
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
}
