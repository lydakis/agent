//! The audit trail: one append-only row per tool call of every bot, and one
//! per step a client runs inside a call, kept apart from the turns they
//! came from, so neither a bot's removal nor a prune takes them. A row is
//! written inside the commit its call already makes and holds only what
//! the daemon saw or a tool declared: its tool, what kind of tool that is,
//! where it ran, who let it run, whether it reads or writes, how it ended,
//! and the start of its arguments. Read newest first by id, which is time
//! order; trimmed only by a request on the daemon's own socket.
use crate::{Error, Result, fail_with};
use rusqlite::{Connection, OptionalExtension, params, types::Value as Sql};
use serde::Deserialize;
use serde_json::{Value, json};

/// What kind of tool a call is: the daemon's own by name, a declared one as
/// its client says, a step as its client says.
pub const KINDS: [&str; 7] = [
    "shell", "file", "mcp", "browser", "desktop", "agents", "web",
];
/// Where a call ran: inside its bot's sandbox, on this machine outside it,
/// or at a service elsewhere.
pub const PLACES: [&str; 3] = ["sandbox", "outside", "external"];
/// Who decided whether a call ran: nobody (nothing asked), a person (an
/// answer given outside the gate's serving session), a reviewer (the
/// gate's serving session, under its lease), or a command rule.
pub const APPROVERS: [&str; 4] = ["nobody", "person", "reviewer", "rule"];
/// What a call does to what it touches, where its tool says: `unknown`
/// for a command, or a tool that says nothing.
pub const ACCESS: [&str; 3] = ["read", "write", "unknown"];
/// How a call ended: `unknown` until it is told, and for good when it never
/// is (a crash, a cancelled run, a client that went away).
pub const OUTCOMES: [&str; 4] = ["completed", "failed", "denied", "unknown"];
/// How much of a call's arguments a row keeps, in characters; the whole
/// call is in its bot's history, under its turn and call id.
pub const PREVIEW_CHARS: usize = 256;
/// The most rows one read returns.
pub const PAGE: usize = 500;
/// Rows a trim deletes per job, so other bots' work interleaves.
pub const TRIM_PIECE: usize = 10_000;

/// What a call's row says it is, besides who allowed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Facts {
    pub kind: Option<&'static str>,
    pub place: Option<&'static str>,
    pub access: &'static str,
}
impl Default for Facts {
    fn default() -> Facts {
        Facts {
            kind: None,
            place: None,
            access: "unknown",
        }
    }
}

impl Facts {
    /// A step's facts as its client gives them, each checked against its
    /// vocabulary; a step that says nothing of what it touches is
    /// `unknown`.
    pub fn given(kind: Option<&str>, place: Option<&str>, access: Option<&str>) -> Result<Facts> {
        Ok(Facts {
            kind: kind.map(|k| word("kind", &KINDS, k)).transpose()?,
            place: place.map(|p| word("place", &PLACES, p)).transpose()?,
            access: access
                .map(|a| word("access", &ACCESS, a))
                .transpose()?
                .unwrap_or("unknown"),
        })
    }
    /// As kept with a step waiting on its gates.
    pub fn to_json(self) -> Value {
        json!({"kind": self.kind, "place": self.place, "access": self.access})
    }
    /// As kept: what was checked when it was given.
    pub fn from_json(kept: &Value) -> Facts {
        let of = |field: &str, values: &[&'static str]| {
            kept[field]
                .as_str()
                .and_then(|given| values.iter().copied().find(|v| *v == given))
        };
        Facts {
            kind: of("kind", &KINDS),
            place: of("place", &PLACES),
            access: of("access", &ACCESS).unwrap_or("unknown"),
        }
    }
}

/// One of `values`, as the daemon's own static name for it.
pub fn word(field: &str, values: &[&'static str], given: &str) -> Result<&'static str> {
    values.iter().copied().find(|v| *v == given).ok_or_else(|| {
        Error::with(
            "invalid_audit",
            format!("{field} is one of {}", values.join(", ")),
        )
    })
}

/// The kind and access of the daemon's own tool `name`.
pub fn daemon_tool(name: &str) -> Option<(&'static str, &'static str)> {
    Some(match name {
        "shell" | "write_stdin" => ("shell", "unknown"),
        "read" => ("file", "read"),
        "write" | "edit" | "apply_patch" => ("file", "write"),
        "web_search" | "web_fetch" => ("web", "read"),
        "wait" | "history" | "echo" => ("agents", "read"),
        _ => return None,
    })
}

/// The facts of a call of the daemon's tool or a declared one: `confined`
/// when it ran under its bot's sandbox. A declared tool's kind is its
/// client's word; where it ran and what it touched are its steps'.
pub fn of_call(name: &str, declared_kind: Option<&str>, confined: bool) -> Facts {
    if let Some((kind, access)) = daemon_tool(name) {
        let place = match kind {
            "shell" | "file" => Some(if confined { "sandbox" } else { "outside" }),
            "web" => Some("external"),
            _ => None,
        };
        return Facts {
            kind: Some(kind),
            place,
            access,
        };
    }
    Facts {
        kind: declared_kind.and_then(|kind| KINDS.iter().copied().find(|k| *k == kind)),
        place: None,
        access: "unknown",
    }
}

/// The start of `text`, at most [`PREVIEW_CHARS`] characters.
pub fn preview(text: &str) -> &str {
    match text.char_indices().nth(PREVIEW_CHARS) {
        Some((end, _)) => &text[..end],
        None => text,
    }
}

/// A new row, its outcome `unknown` until told. Its time never runs back
/// past the row before it, as an event's doesn't, so id order is time
/// order and a time bound is found by id.
#[allow(clippy::too_many_arguments)]
pub fn insert(
    conn: &Connection,
    at_ms: i64,
    bot_id: i64,
    bot: &str,
    turn: i64,
    call_id: &str,
    step: Option<&str>,
    tool: &str,
    facts: Facts,
    approved: &str,
    outcome: &str,
    preview_of: &str,
) -> Result<i64> {
    conn.prepare_cached(
        "INSERT INTO audit(at_ms,bot_id,bot,turn,call_id,step,tool,kind,place,approved,access,outcome,preview)
         VALUES (MAX(?1,COALESCE((SELECT at_ms FROM audit WHERE id=(SELECT MAX(id) FROM audit)),?1)),
            ?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
    )?
    .execute(params![
        at_ms,
        bot_id,
        bot,
        turn,
        call_id,
        step,
        tool,
        facts.kind,
        facts.place,
        approved,
        facts.access,
        outcome,
        preview(preview_of),
    ])?;
    Ok(conn.last_insert_rowid())
}

/// Record how the call of row `id` ended, once: a row already told keeps
/// what it was told first.
pub fn finish(conn: &Connection, id: i64, now_ms: i64, failed: bool) -> Result<()> {
    conn.prepare_cached(
        "UPDATE audit SET outcome=?2,ms=MAX(?3-at_ms,0) WHERE id=?1 AND outcome='unknown'",
    )?
    .execute(params![
        id,
        if failed { "failed" } else { "completed" },
        now_ms
    ])?;
    Ok(())
}

/// What a read of the trail asks for: newest first, older than `before`
/// (an id), within `since_ms`..`until_ms`, and only rows that match every
/// field given; `bot` may be a pattern of * ? and [...].
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub before: Option<i64>,
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    pub bot: Option<String>,
    pub bot_id: Option<i64>,
    pub turn: Option<i64>,
    pub kind: Option<String>,
    pub place: Option<String>,
    pub approved: Option<String>,
    pub access: Option<String>,
    pub outcome: Option<String>,
    pub tool: Option<String>,
    pub limit: Option<usize>,
}

impl Query {
    /// Refuse a field outside its vocabulary, so a misspelled filter is an
    /// error and never a silent empty page.
    pub fn check(&self) -> Result<()> {
        let checks: [(&str, &[&'static str], &Option<String>); 5] = [
            ("kind", &KINDS, &self.kind),
            ("place", &PLACES, &self.place),
            ("approved", &APPROVERS, &self.approved),
            ("access", &ACCESS, &self.access),
            ("outcome", &OUTCOMES, &self.outcome),
        ];
        for (field, values, given) in checks {
            if let Some(given) = given {
                word(field, values, given)?;
            }
        }
        if self.limit.is_some_and(|limit| limit == 0 || limit > PAGE) {
            return fail_with("invalid_audit", format!("limit is 1 to {PAGE}"));
        }
        Ok(())
    }
}

/// The smallest id whose row is at or after `ms`, by bisection over ids:
/// rows' times rise with their ids. One past the newest when none is.
fn first_at(conn: &Connection, ms: i64) -> Result<i64> {
    let bounds: Option<(i64, i64)> = conn.query_row(
        "SELECT (SELECT MIN(id) FROM audit),(SELECT MAX(id) FROM audit)",
        [],
        |r| Ok(r.get::<_, Option<i64>>(0)?.zip(r.get::<_, Option<i64>>(1)?)),
    )?;
    let Some((mut low, high)) = bounds else {
        return Ok(1);
    };
    // Every row below `low` is older than `ms`, and every row at or past
    // `high` is not. Ids can have gaps (a trim, a write rolled back): each
    // probe reads the first row at or past its middle.
    let mut high = high + 1;
    let mut at =
        conn.prepare_cached("SELECT id,at_ms FROM audit WHERE id>=? ORDER BY id LIMIT 1")?;
    while low < high {
        let middle = low + (high - low) / 2;
        let row: Option<(i64, i64)> = at
            .query_row([middle], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        match row {
            Some((id, _)) if id >= high => high = middle,
            None => high = middle,
            Some((id, row_ms)) if row_ms < ms => low = id + 1,
            Some((id, _)) => high = id,
        }
    }
    Ok(low)
}

/// A page of the trail, newest first, and `next_before` to read older rows
/// when there may be more.
pub fn read(conn: &Connection, query: &Query) -> Result<Value> {
    query.check()?;
    let limit = query.limit.unwrap_or(100);
    let mut sql = String::from(
        "SELECT id,at_ms,bot_id,bot,turn,call_id,step,tool,kind,place,approved,access,ms,outcome,preview
         FROM audit WHERE 1",
    );
    let mut args: Vec<Sql> = Vec::new();
    let mut before = query.before;
    if let Some(until) = query.until_ms {
        let bound = first_at(conn, until)?;
        before = Some(before.map_or(bound, |before| before.min(bound)));
    }
    if let Some(before) = before {
        sql.push_str(" AND id<?");
        args.push(Sql::Integer(before));
    }
    if let Some(since) = query.since_ms {
        sql.push_str(" AND id>=?");
        args.push(Sql::Integer(first_at(conn, since)?));
    }
    // A bot by name or by a pattern of * ? and [...], as history's
    // search takes one: no name holds those.
    if let Some(bot) = &query.bot {
        sql.push_str(" AND bot GLOB ?");
        args.push(Sql::Text(bot.clone()));
    }
    let text = [
        ("kind", &query.kind),
        ("place", &query.place),
        ("approved", &query.approved),
        ("access", &query.access),
        ("outcome", &query.outcome),
        ("tool", &query.tool),
    ];
    for (column, value) in text {
        if let Some(value) = value {
            sql.push_str(&format!(" AND {column}=?"));
            args.push(Sql::Text(value.clone()));
        }
    }
    for (column, value) in [("bot_id", query.bot_id), ("turn", query.turn)] {
        if let Some(value) = value {
            sql.push_str(&format!(" AND {column}=?"));
            args.push(Sql::Integer(value));
        }
    }
    sql.push_str(" ORDER BY id DESC LIMIT ?");
    args.push(Sql::Integer(limit as i64 + 1));
    let mut statement = conn.prepare(&sql)?;
    let mut rows: Vec<Value> = statement
        .query_map(rusqlite::params_from_iter(args), |r| {
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "at_ms": r.get::<_, i64>(1)?,
                "bot_id": r.get::<_, i64>(2)?,
                "bot": r.get::<_, String>(3)?,
                "turn": r.get::<_, i64>(4)?,
                "call_id": r.get::<_, String>(5)?,
                "step": r.get::<_, Option<String>>(6)?,
                "tool": r.get::<_, String>(7)?,
                "kind": r.get::<_, Option<String>>(8)?,
                "place": r.get::<_, Option<String>>(9)?,
                "approved": r.get::<_, String>(10)?,
                "access": r.get::<_, String>(11)?,
                "ms": r.get::<_, Option<i64>>(12)?,
                "outcome": r.get::<_, String>(13)?,
                "preview": r.get::<_, String>(14)?,
            }))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let more = rows.len() > limit;
    rows.truncate(limit);
    let next_before = match more {
        true => rows.last().map(|row| row["id"].clone()),
        false => None,
    };
    Ok(json!({"rows": rows, "next_before": next_before}))
}

/// Delete up to [`TRIM_PIECE`] of the oldest rows below id `before`: how
/// many went. A trim is a series of these until one deletes none.
pub fn trim(conn: &Connection, before: i64) -> Result<usize> {
    Ok(conn
        .prepare_cached(
            "DELETE FROM audit WHERE id IN (SELECT id FROM audit WHERE id<? ORDER BY id LIMIT ?)",
        )?
        .execute(params![before, TRIM_PIECE as i64])?)
}

/// The id below which a trim to `before_ms` deletes: the first row at or
/// after that time.
pub fn trim_bound(conn: &Connection, before: Option<i64>, before_ms: Option<i64>) -> Result<i64> {
    match (before, before_ms) {
        (Some(id), None) => Ok(id),
        (None, Some(ms)) => first_at(conn, ms),
        _ => fail_with(
            "invalid_audit",
            "a trim names exactly one of before (an id) or before_ms",
        ),
    }
}
