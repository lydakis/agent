//! A pick on a question the person already answered is never sent, from
//! any surface: a window, the `!` that runs a step, or a client over ACP.
//!
//! A pick checks its question only as it goes. A surface whose events
//! keep a [`needs::Needs`] that holds the question open, under the id its
//! card names, sends at once, with nothing read. Anything else (no such
//! record, its agent's question unsure since an event, or a card named
//! by its call alone) reads that one agent's history for where the
//! question stands before anything is sent: answered already, nothing
//! goes, and the card says so. Every answer that goes names its ask whole
//! (`ask:TURN/CALL`) and goes under [`asks::request_id`], so the daemon
//! takes one answer to an ask whichever surface sends it.
use agent_client::Client;
use serde_json::Value;

use crate::{asks, needs};

/// What a pick about to go does.
#[derive(Debug, PartialEq)]
pub enum Verdict {
    /// Goes, as this ask names its question whole: open or closed by its
    /// agent, whose options still answer it.
    Send(Box<asks::Ask>),
    /// The person answered it already, with these words: nothing goes.
    Answered(String),
    /// Named by its call alone, it is no question its agent has open
    /// now, or where it stands could not be read: nothing goes, since an
    /// answer naming no turn answers nothing.
    Unknown(String),
}

impl Verdict {
    /// Why nothing went, as the notice line says it; none when it goes.
    pub fn why(&self) -> Option<String> {
        match self {
            Verdict::Send(_) => None,
            Verdict::Answered(_) => Some("that question was answered already".into()),
            Verdict::Unknown(why) => Some(why.clone()),
        }
    }
}

/// Whether `needs`, kept by its surface's events, holds ask `id` as
/// `bot`'s open question, read whole and sure since: a pick on it goes at
/// once, nothing read. A card named by its call alone is never held so.
pub fn held_open(needs: &needs::Needs, bot: &str, id: &str) -> bool {
    !needs.unsure(bot) && (needs.asks()).any(|(asker, open)| asker == bot && open.id == id)
}

/// Where `bot`'s ask `ask`, as a card names it, stands now, read from its
/// history: one read of that agent, as a pick goes. A pick goes unless
/// the person answered it already; one named whole goes too when the read
/// fails, since the daemon refuses a second answer under its request id.
pub async fn check(client: &Client, bot: &str, ask: &asks::Ask) -> Verdict {
    let named = (ask.id.strip_prefix("ask:"))
        .and_then(|rest| rest.split_once('/'))
        .and_then(|(turn, call)| Some((turn.parse::<i64>().ok()?, call)));
    let read = match named {
        Some((turn, call)) => asks::standing(client, bot, turn, call, None).await,
        None => by_call(client, bot, ask).await,
    };
    match (read, named.is_some()) {
        (Ok(Some((_, asks::State::Answered(answer)))), _) => Verdict::Answered(answer),
        (Ok(Some((mut found, _))), _) => {
            found.task = ask.task.clone();
            Verdict::Send(Box::new(found))
        }
        (Ok(None) | Err(_), true) => Verdict::Send(Box::new(ask.clone())),
        (Ok(None), false) => Verdict::Unknown("that question is no longer open".into()),
        (Err(error), false) => Verdict::Unknown(format!("could not read that question: {error}")),
    }
}

/// The ask `bot`'s history holds that `card`, drawn with no turn, shows,
/// and where it stands, read back from its newest history: the newest put
/// by the card's call that asks what the card asks, word for word. A
/// provider's call ids are unique only within a turn, so another turn's
/// call of the same id asking anything else is another question, read
/// past, and never answered for this card.
async fn by_call(
    client: &Client,
    bot: &str,
    card: &asks::Ask,
) -> agent_client::Result<Option<(asks::Ask, asks::State)>> {
    let (mut asked, mut found) = (asks::Asked::default(), None);
    crate::said::walk(client, bot, 0, |at, entry: &Value| {
        found = (asked.entry(at, entry).into_iter()).find(|(ask, _)| {
            ask.id.rsplit_once('/').is_some_and(|(_, of)| of == card.id) && same(ask, card)
        });
        found.is_some()
    })
    .await?;
    Ok(found)
}

/// Whether two asks put the same question to the person: its words, why,
/// options, recommendation and what a yes covers.
fn same(one: &asks::Ask, other: &asks::Ask) -> bool {
    (one.question == other.question)
        && (one.why == other.why)
        && (one.options == other.options)
        && (one.recommended == other.recommended)
        && (one.covers == other.covers)
}

/// `prompt`, an answer [`asks::answer`] wrote to ask `was`, naming ask
/// `now` instead: the same question, named whole.
pub fn renamed(prompt: &str, was: &str, now: &str) -> String {
    let named = |id: &str| format!("You asked ({id}): ");
    match prompt.strip_prefix(&named(was)) {
        Some(rest) => format!("{}{rest}", named(now)),
        None => prompt.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ask(id: &str) -> asks::Ask {
        let arguments =
            json!({"question": "Ship it?", "options": [{"label": "Now"}, {"label": "Later"}]});
        asks::call(id, &arguments).unwrap()
    }

    #[test]
    fn needs_hold_a_question_open_only_whole_and_sure() {
        let mut needs = needs::Needs::default();
        assert!(!held_open(&needs, "shop.lead", "ask:4/c1"), "never read");
        needs.asked("shop.lead", Some(ask("ask:4/c1")));
        assert!(held_open(&needs, "shop.lead", "ask:4/c1"));
        assert!(!held_open(&needs, "shop.lead", "c1"), "its call alone");
        assert!(
            !held_open(&needs, "shop.lead", "ask:3/c1"),
            "another turn's"
        );
        assert!(
            !held_open(&needs, "shop.fix", "ask:4/c1"),
            "another agent's"
        );
        needs.read_at_end("shop.lead");
        assert!(!held_open(&needs, "shop.lead", "ask:4/c1"), "unsure since");
        needs.asked("shop.lead", None);
        assert!(!held_open(&needs, "shop.lead", "ask:4/c1"), "answered");
    }

    /// A card named by its call alone is the question it shows: another
    /// turn's call of the same id asking something else is never answered
    /// for it, nor is that question's state taken for the card's.
    #[tokio::test]
    async fn a_card_with_no_turn_is_matched_by_what_it_asks_too() {
        let call = |question: &str| {
            let arguments = json!({"question": question,
                "options": [{"label": "Now"}, {"label": "Later"}]});
            json!({"type": "function_call", "name": asks::TOOL, "call_id": "c1",
                "arguments": arguments.to_string()})
        };
        let taken = json!({"type": "function_call_output", "call_id": "c1", "output": "{}"});
        let said = json!({"type": "message", "role": "user", "content": [{"type": "input_text",
            "text": "You asked (ask:4/c1): Ship it?\n\nNow"}]});
        // Newest first: turn 6 asks "Tag it?" under the same call id.
        let history = vec![
            (21, 6, taken.clone()),
            (20, 6, call("Tag it?")),
            (18, 5, said),
            (17, 4, taken),
            (16, 4, call("Ship it?")),
        ];
        let client = crate::tests::daemon_answering(move |request| {
            crate::tests::history_read(&history, request).unwrap_or_default()
        })
        .await;
        let card = |question: &str| {
            let arguments = json!({"question": question,
                "options": [{"label": "Now"}, {"label": "Later"}]});
            asks::call("c1", &arguments).unwrap()
        };
        assert_eq!(
            check(&client, "shop.lead", &card("Ship it?")).await,
            Verdict::Answered("Now".into())
        );
        match check(&client, "shop.lead", &card("Tag it?")).await {
            Verdict::Send(ask) => assert_eq!(ask.id, "ask:6/c1"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            check(&client, "shop.lead", &card("Merge it?")).await,
            Verdict::Unknown(_)
        ));
    }

    #[test]
    fn an_answer_is_named_again_whole() {
        let short = asks::answer(&ask("c1"), "2");
        assert_eq!(
            renamed(&short, "c1", "ask:4/c1"),
            asks::answer(&ask("ask:4/c1"), "2")
        );
        assert_eq!(renamed("Later", "c1", "ask:4/c1"), "Later");
    }
}
