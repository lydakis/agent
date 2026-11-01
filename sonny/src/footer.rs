//! The row under the composer: on the left what the person is talking to
//! and where, on the right what wants their attention. Each item comes
//! in forms from long to short, and when the row is too long the next
//! step of [`LADDER`] takes an item to its next form, as Powerline drops
//! its lowest-priority segments first: words before marks, marks before
//! whole items, and what needs the person never.
use ratatui::style::{Color, Stylize};
use ratatui::text::{Line, Span};
use std::ops::Range;
use unicode_width::UnicodeWidthStr;

/// Where an item sits, in its side's order; the left side first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Slot {
    /// The permissions row, when not the one new leads start in.
    Access,
    /// The model with its effort, boost and speed.
    Model,
    /// What waits for the turn to end.
    Pending,
    /// The profile, when not the seat's own.
    Profile,
    /// The folder and its branch.
    Place,
    /// A side chat, said for as long as it is open.
    Side,
    Needs,
    Failed,
    Working,
    Background,
    Notes,
    /// The context while under half of it is used: good to know.
    RoomyContext,
    /// The context once half of it is used: it never goes.
    LowContext,
}

impl Slot {
    fn left(self) -> bool {
        self <= Slot::Place
    }
}

/// The order things give way: each step takes that item to its next
/// form, and a step for an item absent or at its last form is passed.
const LADDER: [Slot; 16] = [
    Slot::Background,
    Slot::Access,
    Slot::Place,
    Slot::RoomyContext,
    Slot::Working,
    Slot::Place,
    Slot::Notes,
    Slot::Background,
    Slot::Failed,
    Slot::Needs,
    Slot::LowContext,
    Slot::Place,
    Slot::Profile,
    Slot::Model,
    Slot::Working,
    Slot::Pending,
];

/// One item: its forms from long to short, an empty one for gone; and
/// what a click on it opens, as a button link names it.
pub struct Item {
    pub slot: Slot,
    pub forms: Vec<Vec<Span<'static>>>,
    pub opens: Option<&'static str>,
}

impl Item {
    pub fn new(slot: Slot, forms: Vec<Vec<Span<'static>>>) -> Self {
        Item {
            slot,
            forms,
            opens: None,
        }
    }

    pub fn opens(mut self, link: &'static str) -> Self {
        self.opens = Some(link);
        self
    }
}

/// A count as the state table writes it: its mark in its hue, then the
/// number and the word; the number alone when short, and gone last when
/// `goes`.
pub fn count(
    mark: &'static str,
    color: Color,
    n: usize,
    word: &str,
    goes: bool,
) -> Vec<Vec<Span<'static>>> {
    let mut forms = vec![
        vec![
            Span::raw(mark).fg(color),
            Span::raw(format!(" {n} {word}")).dim(),
        ],
        vec![Span::raw(mark).fg(color), Span::raw(format!(" {n}")).dim()],
    ];
    if goes {
        forms.push(Vec::new());
    }
    forms
}

/// A folder as fish's prompt shortens it: each part but the last to its
/// first character, a dot kept with the one after it.
pub fn fish(folder: &str) -> String {
    let parts: Vec<&str> = folder.split('/').collect();
    let last = parts.len().saturating_sub(1);
    (parts.iter().enumerate())
        .map(
            |(i, part)| match i == last || part.is_empty() || *part == "~" {
                true => (*part).to_owned(),
                false => {
                    let n = if part.starts_with('.') { 2 } else { 1 };
                    part.chars().take(n).collect()
                }
            },
        )
        .collect::<Vec<_>>()
        .join("/")
}

/// The last part of a folder, what tells it apart.
pub fn base(folder: &str) -> &str {
    match folder.trim_end_matches('/').rsplit('/').next() {
        Some(last) if !last.is_empty() => last,
        _ => folder,
    }
}

fn width(spans: &[Span]) -> usize {
    spans.iter().map(|span| span.content.width()).sum()
}

/// The row at `width`, both ends inset two columns, and the columns each
/// item with somewhere to open covers, with what it opens.
pub fn lay(mut items: Vec<Item>, width: usize) -> (Line<'static>, Vec<(Range<u16>, &'static str)>) {
    items.sort_by_key(|item| item.slot);
    let mut at = vec![0; items.len()];
    let room = width.saturating_sub(4);
    let mut steps = LADDER.iter();
    loop {
        let (left, right) = sides(&items, &at);
        let used = self::width(&left) + self::width(&right) + if right.is_empty() { 0 } else { 2 };
        if used <= room {
            break;
        }
        let Some(step) = steps.next() else { break };
        if let Some(i) = items.iter().position(|item| item.slot == *step)
            && at[i] + 1 < items[i].forms.len()
        {
            at[i] += 1;
        }
    }
    let (left, right) = sides(&items, &at);
    let fill = room
        .saturating_sub(self::width(&left) + self::width(&right))
        .max(match right.is_empty() {
            true => 0,
            false => 2,
        });
    let mut spans = vec![Span::raw("  ")];
    spans.extend(left);
    spans.push(Span::raw(" ".repeat(fill)));
    // Where each linked item lands, counted as the spans are laid.
    let mut opens = Vec::new();
    let mut column = self::width(&spans);
    let mut first = true;
    for (item, form) in items.iter().zip(&at).filter(|(item, _)| !item.slot.left()) {
        let shown = &item.forms[*form];
        if shown.is_empty() {
            continue;
        }
        if !first {
            spans.push(Span::raw("  "));
            column += 2;
        }
        first = false;
        let w = self::width(shown);
        if let Some(link) = item.opens {
            opens.push((column as u16..(column + w) as u16, link));
        }
        column += w;
        spans.extend(shown.iter().cloned());
    }
    spans.push(Span::raw("  "));
    (Line::from(spans), opens)
}

/// The left side, its items between ` · `, a mark alone followed by a
/// space only; and the right, its items starting with their own marks,
/// between two spaces.
fn sides(items: &[Item], at: &[usize]) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
    let (mut left, mut right) = (Vec::new(), Vec::new());
    let mut lone_mark = false;
    for (item, form) in items.iter().zip(at) {
        let shown = &item.forms[*form];
        if shown.is_empty() {
            continue;
        }
        let side = if item.slot.left() {
            &mut left
        } else {
            &mut right
        };
        if !side.is_empty() {
            side.push(match (item.slot.left(), lone_mark) {
                (true, true) => Span::raw(" "),
                (true, false) => Span::raw(" · ").dim(),
                (false, _) => Span::raw("  "),
            });
        }
        lone_mark = item.slot == Slot::Access && *form + 1 == item.forms.len() && shown.len() == 1;
        side.extend(shown.iter().cloned());
    }
    (left, right)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(spans: &[Span]) -> String {
        spans.iter().map(|span| span.content.as_ref()).collect()
    }

    fn word(slot: Slot, forms: &[&str]) -> Item {
        let forms = forms
            .iter()
            .map(|form| match form.is_empty() {
                true => Vec::new(),
                false => vec![Span::raw(form.to_string()).dim()],
            })
            .collect();
        Item::new(slot, forms)
    }

    /// A busy lead, as on the proposal's mockups.
    fn busy() -> Vec<Item> {
        vec![
            Item::new(
                Slot::Access,
                vec![
                    vec![Span::raw("⏵⏵"), Span::raw(" full access").dim()],
                    vec![Span::raw("⏵⏵")],
                ],
            ),
            word(Slot::Model, &["GPT 6.1 Sol high", "GPT 6.1 Sol"]),
            word(
                Slot::Place,
                &[
                    "~/Developer/agent on main",
                    "~/D/agent on main",
                    "agent on main",
                    "",
                ],
            ),
            Item::new(
                Slot::Needs,
                count("?", Color::Yellow, 1, "needs you", false),
            )
            .opens("status"),
            Item::new(Slot::Failed, count("✗", Color::Red, 1, "failed", false)).opens("background"),
            Item::new(Slot::Working, count("●", Color::Cyan, 2, "working", true)).opens("status"),
            word(Slot::Background, &["↻ 3 in background", "↻ 3", ""]),
            word(Slot::Notes, &["✎ 2", ""]),
            Item::new(
                Slot::LowContext,
                vec![
                    vec![Span::raw("37%").fg(Color::Yellow), Span::raw(" left").dim()],
                    vec![Span::raw("37%").fg(Color::Yellow)],
                ],
            ),
        ]
    }

    fn row(items: Vec<Item>, columns: usize) -> String {
        text(&lay(items, columns).0.spans)
    }

    #[test]
    fn a_busy_row_gives_way_by_the_ladder_and_keeps_what_needs_you() {
        let rows: Vec<String> = [100, 80, 60, 44].map(|w| row(busy(), w)).into();
        assert_eq!(
            rows[0].trim(),
            "⏵⏵ GPT 6.1 Sol high · ~/D/agent on main       ? 1 needs you  ✗ 1 failed  ● 2  ↻ 3  ✎ 2  37% left"
        );
        assert_eq!(
            rows[1].trim(),
            "⏵⏵ GPT 6.1 Sol high · agent on main        ? 1 needs you  ✗ 1  ● 2  37% left"
        );
        assert_eq!(
            rows[2].trim(),
            "⏵⏵ GPT 6.1 Sol high · agent on main   ? 1  ✗ 1  ● 2  37%"
        );
        assert_eq!(rows[3].trim(), "⏵⏵ GPT 6.1 Sol high   ? 1  ✗ 1  ● 2  37%");
        for (row, w) in rows.iter().zip([100, 80, 60, 44]) {
            assert_eq!(row.width(), w, "{row}");
            assert!(row.starts_with("  ⏵⏵") && row.ends_with("  "), "{row}");
        }
    }

    #[test]
    fn a_quiet_row_keeps_every_word() {
        let items = vec![
            Item::new(
                Slot::Access,
                vec![
                    vec![Span::raw("⏵⏵"), Span::raw(" full access").dim()],
                    vec![Span::raw("⏵⏵")],
                ],
            ),
            word(Slot::Model, &["GPT 6.1 Sol high", "GPT 6.1 Sol"]),
            word(
                Slot::Place,
                &[
                    "~/Developer/agent on main",
                    "~/D/agent on main",
                    "agent on main",
                    "",
                ],
            ),
            word(Slot::Notes, &["✎ 2", ""]),
            word(Slot::LowContext, &["37% left", "37%"]),
        ];
        let line = row(items, 100);
        assert!(
            line.starts_with("  ⏵⏵ full access · GPT 6.1 Sol high · ~/Developer/agent on main  "),
            "{line}"
        );
        assert!(line.ends_with("  ✎ 2  37% left  "), "{line}");
    }

    #[test]
    fn a_click_lands_on_the_count_it_opens() {
        let (line, opens) = lay(busy(), 100);
        let row = text(&line.spans);
        let shown = |range: &Range<u16>| -> String {
            let mut column = 0;
            row.chars()
                .filter(|c| {
                    let at = column;
                    column += c.to_string().width();
                    range.contains(&(at as u16))
                })
                .collect()
        };
        let got: Vec<(String, &str)> = opens.iter().map(|(r, l)| (shown(r), *l)).collect();
        assert_eq!(
            got,
            [
                ("? 1 needs you".into(), "status"),
                ("✗ 1 failed".into(), "background"),
                ("● 2".into(), "status"),
            ]
        );
    }

    #[test]
    fn folders_shorten_by_whole_parts() {
        assert_eq!(fish("~/Developer/agent"), "~/D/agent");
        assert_eq!(fish("/work/.agent/worktrees/shop"), "/w/.a/w/shop");
        assert_eq!(fish("~"), "~");
        assert_eq!(base("~/Developer/agent"), "agent");
        assert_eq!(base("/"), "/");
    }
}
