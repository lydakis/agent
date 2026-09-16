use crate::{
    Error, Result,
    codec::Family,
    fail, fail_with,
    provider::{ToolCall, Usage},
    tools::Outcome,
};
use bytes::Bytes;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Debug, Clone, Serialize)]
pub struct Bot {
    pub name: String,
    pub head: Option<i64>,
    /// Lifetime cap on input plus output tokens; checked before each model call.
    pub budget_tokens: Option<u64>,
    pub tokens_used: u64,
    /// Default directory for submissions that name none; a bot need not have one.
    pub workspace: Option<String>,
    pub status: String,
    pub running_turn: Option<i64>,
    pub provider: String,
    pub family: String,
    pub model: String,
    pub instructions: String,
    pub reasoning: Option<String>,
}
impl Bot {
    pub fn family(&self) -> Result<Family> {
        Family::parse(&self.family).ok_or(Error::new("store_family_unsupported"))
    }
}
/// Provider binding chosen at creation; immutable for the bot's lifetime.
pub struct Binding<'a> {
    pub provider: &'a str,
    pub family: Family,
    pub model: &'a str,
    pub instructions: &'a str,
    pub reasoning: Option<&'a str>,
    pub budget_tokens: Option<u64>,
}
#[derive(Debug)]
pub struct Started {
    pub turn: i64,
    pub fresh: bool,
    /// The durable `accepted` entry, present only for fresh submissions.
    pub entry: Option<Value>,
}
/// Per-turn overrides of the bot's defaults, recorded with the turn.
#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct TurnOptions {
    pub workspace: Option<String>,
    pub model: Option<String>,
}
/// A turn parked on handles; it holds no task or memory until they resolve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct Waiting {
    pub turn: i64,
    pub bot: String,
    pub call_id: String,
    pub handles: Vec<String>,
    pub deadline_ms: Option<u64>,
    /// Tool calls from the same model response that follow the wait.
    pub pending: Vec<ToolCall>,
}
/// The bounded request context: ordered node ids and exact item bytes.
#[derive(Debug)]
pub struct Window {
    pub family: Family,
    pub ids: Vec<i64>,
    /// Sum of item lengths, without separators.
    pub item_bytes: i64,
    pub omitted_items: i64,
    pub omitted_turns: i64,
}
/// Where and with which model a turn runs.
pub struct TurnContext {
    pub model_rounds: usize,
    pub bot: String,
    pub workspace: String,
    pub model: String,
}
pub struct Database {
    conn: Connection,
}

/// Durable events and their live copies share one shape.
fn entry(cursor: i64, bot: &str, turn: Option<i64>, kind: &str, data: Value) -> Value {
    json!({"cursor":cursor,"bot":bot,"turn":turn,"event":kind,"data":data})
}

impl Database {
    /// Stored schema version, kept in `PRAGMA user_version`. Stores created
    /// before versioning and stores from newer binaries are rejected; an older
    /// versioned store is migrated forward, one version at a time, at open.
    pub const SCHEMA: i32 = 12;

    pub fn initialize(conn: Connection, configuration: &str) -> Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            PRAGMA foreign_keys=ON; PRAGMA cache_size=-2048;",
        )?;
        let version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        let has_tables: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='bots')",
            [],
            |r| r.get(0),
        )?;
        // Initialize a fresh store atomically; existing stores must match.
        let tx = conn.unchecked_transaction()?;
        match version {
            0 if has_tables => {
                return fail_with(
                    "store_schema_unsupported",
                    "created before schema versioning; start a new store",
                );
            }
            v if v > Self::SCHEMA => {
                return fail_with(
                    "store_schema_newer",
                    format!("store schema {v}, binary supports {}", Self::SCHEMA),
                );
            }
            v if v != 0 && v < Self::SCHEMA => migrate(&tx, v)?,
            _ => {}
        }
        tx.execute_batch("
            CREATE TABLE IF NOT EXISTS configuration(value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS nodes(id INTEGER PRIMARY KEY, parent INTEGER REFERENCES nodes(id),
                item BLOB NOT NULL, total_bytes INTEGER NOT NULL, depth INTEGER NOT NULL,
                turn INTEGER, turn_seq INTEGER);
            CREATE INDEX IF NOT EXISTS nodes_turn_seq ON nodes(turn_seq) WHERE turn_seq IS NOT NULL;
            CREATE TABLE IF NOT EXISTS node_sequence(singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                last_id INTEGER NOT NULL CHECK(last_id>=0));
            INSERT OR IGNORE INTO node_sequence VALUES (1,0);
            CREATE TABLE IF NOT EXISTS bots(name TEXT PRIMARY KEY, head INTEGER REFERENCES nodes(id),
                workspace TEXT, status TEXT NOT NULL, running_turn INTEGER,
                provider TEXT NOT NULL, family TEXT NOT NULL, model TEXT NOT NULL,
                instructions TEXT NOT NULL, reasoning TEXT,
                budget_tokens INTEGER, tokens_used INTEGER NOT NULL DEFAULT 0,
                context_start INTEGER REFERENCES nodes(id),
                pruned_cursor INTEGER NOT NULL DEFAULT 0);
            CREATE INDEX IF NOT EXISTS nodes_parent ON nodes(parent);
            CREATE INDEX IF NOT EXISTS bots_head ON bots(head);
            CREATE INDEX IF NOT EXISTS bots_context_start ON bots(context_start);
            CREATE TABLE IF NOT EXISTS turns(id INTEGER PRIMARY KEY, bot TEXT NOT NULL REFERENCES bots(name),
                request_id TEXT NOT NULL, prompt TEXT NOT NULL, status TEXT NOT NULL,
                workspace TEXT, model TEXT, waiting TEXT, model_rounds INTEGER NOT NULL DEFAULT 0,
                input_tokens INTEGER NOT NULL DEFAULT 0, output_tokens INTEGER NOT NULL DEFAULT 0,
                started_ms INTEGER, finished_ms INTEGER,
                UNIQUE(bot,request_id));
            CREATE INDEX IF NOT EXISTS turns_bot_id ON turns(bot,id);
            CREATE TABLE IF NOT EXISTS retained_turns(turn INTEGER PRIMARY KEY REFERENCES turns(id) ON DELETE CASCADE,
                bot TEXT NOT NULL REFERENCES bots(name));
            CREATE INDEX IF NOT EXISTS retained_turns_bot ON retained_turns(bot,turn);
            CREATE TABLE IF NOT EXISTS turn_sequence(singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                last_id INTEGER NOT NULL CHECK(last_id>=0));
            INSERT OR IGNORE INTO turn_sequence VALUES (1,0);
            CREATE TABLE IF NOT EXISTS checkpoints(bot TEXT NOT NULL REFERENCES bots(name), head INTEGER NOT NULL REFERENCES nodes(id),
                PRIMARY KEY(bot,head));
            CREATE TABLE IF NOT EXISTS tools(turn INTEGER NOT NULL REFERENCES turns(id), call_id TEXT NOT NULL,
                status TEXT NOT NULL, PRIMARY KEY(turn,call_id));
            CREATE TABLE IF NOT EXISTS processes(id INTEGER PRIMARY KEY AUTOINCREMENT, turn INTEGER NOT NULL REFERENCES turns(id),
                call_id TEXT NOT NULL, status TEXT NOT NULL, result TEXT);
            CREATE INDEX IF NOT EXISTS processes_turn ON processes(turn);
            CREATE TABLE IF NOT EXISTS artifacts(turn INTEGER NOT NULL REFERENCES turns(id), call_id TEXT NOT NULL,
                stream TEXT NOT NULL, data BLOB NOT NULL, PRIMARY KEY(turn,call_id,stream));
            CREATE TABLE IF NOT EXISTS events(id INTEGER PRIMARY KEY AUTOINCREMENT, bot TEXT NOT NULL REFERENCES bots(name),
                turn INTEGER, kind TEXT NOT NULL, data TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS events_bot_cursor ON events(bot,id);
            CREATE INDEX IF NOT EXISTS events_turn_kind_cursor ON events(turn,kind,id);
            CREATE INDEX IF NOT EXISTS checkpoints_head ON checkpoints(head);
            CREATE INDEX IF NOT EXISTS turns_running ON turns(id) WHERE status='running';
            CREATE INDEX IF NOT EXISTS turns_waiting ON turns(id) WHERE status='waiting';
            CREATE INDEX IF NOT EXISTS processes_running ON processes(turn) WHERE status='running';")?;
        if version != Self::SCHEMA {
            tx.pragma_update(None, "user_version", Self::SCHEMA)?;
        }
        let saved: Option<String> = tx
            .query_row("SELECT value FROM configuration", [], |r| r.get(0))
            .optional()?;
        match saved {
            None => {
                tx.execute("INSERT INTO configuration VALUES (?)", [configuration])?;
            }
            Some(saved) if saved != configuration => return fail("store_configuration_mismatch"),
            _ => {}
        }
        tx.commit()?;
        // Background commands died with the previous daemon; their handles
        // must report loss rather than resolve to some later command.
        conn.execute(
            "UPDATE processes SET status='lost',result=? WHERE status='running'",
            [json!({"error":"process_lost"}).to_string()],
        )?;
        let mut db = Self { conn };
        // A committed tool intent without a result is never automatically
        // retried. Turns parked on handles keep their state and resume.
        let pending: Vec<i64> = db
            .conn
            .prepare("SELECT id FROM turns WHERE status='running'")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for turn in pending {
            db.finish(turn, Some(&Error::new("process_interrupted")))?;
        }
        Ok(db)
    }

    fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Bot> {
        Ok(Bot {
            name: r.get(0)?,
            head: r.get(1)?,
            budget_tokens: r.get::<_, Option<i64>>(10)?.map(|b| b.max(0) as u64),
            tokens_used: r.get::<_, i64>(11)?.max(0) as u64,
            workspace: r.get(2)?,
            status: r.get(3)?,
            running_turn: r.get(4)?,
            provider: r.get(5)?,
            family: r.get(6)?,
            model: r.get(7)?,
            instructions: r.get(8)?,
            reasoning: r.get(9)?,
        })
    }
    const COLUMNS: &str = "name,head,workspace,status,running_turn,provider,family,model,instructions,reasoning,budget_tokens,tokens_used";
    pub fn inspect(&self, name: &str) -> Result<Bot> {
        self.conn
            .query_row(
                &format!("SELECT {} FROM bots WHERE name=?", Self::COLUMNS),
                [name],
                Self::row,
            )
            .optional()?
            .ok_or(Error::new("bot_not_found"))
    }
    /// Bounded keyset pages. Full instructions remain available through inspect.
    pub fn list(&self, after: Option<&str>, limit: usize) -> Result<Value> {
        if !(1..=256).contains(&limit) || after.is_some_and(|s| s.len() > 128) {
            return fail("invalid_bot_page");
        }
        let mut statement = self.conn.prepare(
            "SELECT name,head,workspace,status,running_turn,provider,family,model,reasoning,budget_tokens,tokens_used
             FROM bots WHERE name > ? ORDER BY name LIMIT ?",
        )?;
        let mut rows = statement.query(params![after.unwrap_or(""), (limit + 1) as i64])?;
        let mut bots = Vec::new();
        let mut bytes = 0;
        let mut more = false;
        while let Some(r) = rows.next()? {
            let bot = json!({"name":r.get::<_, String>(0)?,"head":r.get::<_, Option<i64>>(1)?,
                "workspace":r.get::<_, Option<String>>(2)?,"status":r.get::<_, String>(3)?,
                "running_turn":r.get::<_, Option<i64>>(4)?,"provider":r.get::<_, String>(5)?,
                "family":r.get::<_, String>(6)?,"model":r.get::<_, String>(7)?,
                "reasoning":r.get::<_, Option<String>>(8)?,
                "budget_tokens":r.get::<_, Option<i64>>(9)?,"tokens_used":r.get::<_, i64>(10)?});
            let size = crate::output::encoded_len(&bot)? + 1;
            if bots.len() == limit || bytes + size > crate::output::MAX_EVENT / 2 {
                if bots.is_empty() {
                    return fail("bot_page_item_limit");
                }
                more = true;
                break;
            }
            bytes += size;
            bots.push(bot);
        }
        let next = more.then(|| bots.last().unwrap()["name"].clone());
        Ok(json!({"bots":bots,"next_after":next}))
    }
    fn exists(&self, name: &str) -> Result<bool> {
        Ok(self
            .conn
            .query_row("SELECT 1 FROM bots WHERE name=?", [name], |_| Ok(()))
            .optional()?
            .is_some())
    }
    pub fn create(
        &mut self,
        name: &str,
        workspace: Option<&str>,
        binding: Binding<'_>,
    ) -> Result<Bot> {
        if self.exists(name)? {
            return fail("bot_exists");
        }
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO bots VALUES (?,NULL,?,'idle',NULL,?,?,?,?,?,?,0,NULL,0)",
            params![
                name,
                workspace,
                binding.provider,
                binding.family.name(),
                binding.model,
                binding.instructions,
                binding.reasoning,
                binding.budget_tokens.map(|b| b as i64)
            ],
        )?;
        event(
            &tx,
            name,
            None,
            "created",
            json!({"model":format!("{}/{}", binding.provider, binding.model)}),
        )?;
        tx.commit()?;
        self.inspect(name)
    }
    /// Is `node` on the path from `head` back to the root?
    fn in_lineage(&self, head: Option<i64>, node: i64) -> Result<bool> {
        let Some(head) = head else {
            return Ok(false);
        };
        let depth: Option<i64> = self
            .conn
            .query_row("SELECT depth FROM nodes WHERE id=?", [node], |r| r.get(0))
            .optional()?;
        let Some(depth) = depth else {
            return Ok(false);
        };
        Ok(self
            .conn
            .query_row(
                "WITH RECURSIVE chain(id,parent,depth) AS (
                    SELECT id,parent,depth FROM nodes WHERE id=?1
                    UNION ALL SELECT n.id,n.parent,n.depth FROM nodes n JOIN chain c ON n.id=c.parent WHERE c.depth>?3)
                 SELECT 1 FROM chain WHERE id=?2 LIMIT 1",
                params![head, node, depth],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }
    /// The bounded model context for the bot's next request: the newest
    /// turns that fit `context_bytes` and `context_items`, starting at a turn
    /// boundary. The start is persisted and only moves when the budget is
    /// exceeded, then jumps back to about three quarters of the budget so the
    /// cached prefix stays stable across many turns.
    pub fn window(
        &mut self,
        name: &str,
        context_bytes: i64,
        context_items: i64,
    ) -> Result<Option<Window>> {
        // Heads only append and forks clear context_start, so the saved start
        // remains in this bot's ancestry. Read its accounting without walking
        // that ancestry or copying unrelated bot metadata such as instructions.
        struct State {
            head: Option<i64>,
            family: String,
            total: i64,
            depth: i64,
            start: Option<i64>,
            start_depth: i64,
            before: i64,
            turn_seq: i64,
        }
        let state = self
            .conn
            .prepare_cached(
                "SELECT b.head,b.family,COALESCE(h.total_bytes,0),COALESCE(h.depth,0),
                    s.id,COALESCE(s.depth,0),COALESCE(p.total_bytes,0),COALESCE(s.turn_seq,1)
             FROM bots b LEFT JOIN nodes h ON h.id=b.head
             LEFT JOIN nodes s ON s.id=b.context_start LEFT JOIN nodes p ON p.id=s.parent
             WHERE b.name=?",
            )?
            .query_row([name], |r| {
                Ok(State {
                    head: r.get(0)?,
                    family: r.get(1)?,
                    total: r.get(2)?,
                    depth: r.get(3)?,
                    start: r.get(4)?,
                    start_depth: r.get(5)?,
                    before: r.get(6)?,
                    turn_seq: r.get(7)?,
                })
            })
            .optional()?
            .ok_or(Error::new("bot_not_found"))?;
        let Some(head) = state.head else {
            return Ok(None);
        };
        let head_total = state.total;
        let head_depth = state.depth;
        let current = state
            .start
            .map(|start| (start, state.start_depth, state.before, state.turn_seq));
        let fits = |depth: i64, before: i64| {
            head_total - before <= context_bytes && head_depth - depth < context_items
        };
        let chosen = match current {
            Some((start, depth, before, seq)) if fits(depth, before) => (start, depth, before, seq),
            _ => {
                // Walk back from the head over turn starts while the tail
                // still fits, keeping the oldest boundary under the target.
                let target_bytes = context_bytes / 4 * 3;
                let target_items = context_items / 4 * 3;
                let mut statement = self.conn.prepare_cached(
                    "WITH RECURSIVE chain(id,parent,depth,total_bytes,turn_seq) AS (
                        SELECT id,parent,depth,total_bytes,turn_seq FROM nodes WHERE id=?1
                        UNION ALL SELECT n.id,n.parent,n.depth,n.total_bytes,n.turn_seq FROM nodes n JOIN chain c ON n.id=c.parent
                        WHERE ?2 - c.total_bytes <= ?3 AND ?4 - c.depth <= ?5)
                     SELECT c.id,c.depth,COALESCE(p.total_bytes,0),c.turn_seq FROM chain c LEFT JOIN nodes p ON p.id=c.parent
                     WHERE c.turn_seq IS NOT NULL ORDER BY c.depth DESC",
                )?;
                let candidates = statement.query_map(
                    params![head, head_total, context_bytes, head_depth, context_items],
                    |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, i64>(1)?,
                            r.get::<_, i64>(2)?,
                            r.get::<_, i64>(3)?,
                        ))
                    },
                )?;
                let mut pick = None;
                for candidate in candidates {
                    let (id, depth, before, seq) = candidate?;
                    if !fits(depth, before) {
                        break;
                    }
                    let within_target =
                        head_total - before <= target_bytes && head_depth - depth < target_items;
                    if within_target || pick.is_none() {
                        pick = Some((id, depth, before, seq));
                    }
                    if !within_target {
                        break;
                    }
                }
                let Some(pick) = pick else {
                    return fail_with(
                        "context_limit",
                        "the current turn alone exceeds the context budget",
                    );
                };
                self.conn.execute(
                    "UPDATE bots SET context_start=? WHERE name=?",
                    params![pick.0, name],
                )?;
                pick
            }
        };
        let (_, start_depth, before, turn_seq) = chosen;
        let mut statement = self.conn.prepare_cached(
            "WITH RECURSIVE chain(id,parent,depth) AS (
                SELECT id,parent,depth FROM nodes WHERE id=?1
                UNION ALL SELECT n.id,n.parent,n.depth FROM nodes n JOIN chain c ON n.id=c.parent WHERE c.depth>?2)
             SELECT id FROM chain ORDER BY depth",
        )?;
        let ids: Vec<i64> = statement
            .query_map(params![head, start_depth], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(Some(Window {
            family: Family::parse(&state.family).ok_or(Error::new("store_family_unsupported"))?,
            ids,
            item_bytes: head_total - before,
            omitted_items: start_depth - 1,
            omitted_turns: turn_seq - 1,
        }))
    }
    /// Encoded items for a batch of window ids, in order, comma-separated.
    pub fn items_by_ids(&self, ids: &[i64]) -> Result<Vec<u8>> {
        let mut statement = self
            .conn
            .prepare_cached("SELECT item FROM nodes WHERE id=?")?;
        let mut out = Vec::new();
        for (index, id) in ids.iter().enumerate() {
            if index != 0 {
                out.push(b',');
            }
            statement.query_row([id], |r| {
                out.extend_from_slice(r.get_ref(0)?.as_blob()?);
                Ok(())
            })?;
        }
        Ok(out)
    }
    /// Bytes and items in the active turn alone. Older turns can be removed
    /// from the context window; the current turn cannot. Walk metadata only.
    pub fn turn_usage(&self, name: &str, turn: i64) -> Result<(Family, usize, usize)> {
        let row: Option<(String, i64, i64)> = self
            .conn
            .prepare_cached(
                "WITH RECURSIVE chain(id,parent,turn) AS (
                SELECT n.id,n.parent,n.turn FROM bots b JOIN nodes n ON n.id=b.head
                WHERE b.name=?1 AND b.running_turn=?2
                UNION ALL SELECT n.id,n.parent,n.turn FROM nodes n JOIN chain c ON n.id=c.parent
                WHERE c.turn IS NULL)
             SELECT b.family,h.total_bytes-COALESCE(p.total_bytes,0),h.depth-COALESCE(p.depth,0)
             FROM chain c JOIN bots b ON b.name=?1 JOIN nodes h ON h.id=b.head
             LEFT JOIN nodes p ON p.id=c.parent WHERE c.turn=?2",
            )?
            .query_row(params![name, turn], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .optional()?;
        let Some((family, bytes, items)) = row else {
            return fail("stale_turn");
        };
        Ok((
            Family::parse(&family).ok_or(Error::new("store_family_unsupported"))?,
            bytes as usize,
            items as usize,
        ))
    }

    /// Page a reading view of provider items as JSONL by byte offset in that
    /// view. Only reasoning.encrypted_content is omitted; stored items and
    /// provider replay remain unchanged.
    /// Pages end at UTF-8 boundaries and may split a JSON record. Concatenate
    /// their text before decoding; no content is replaced with previews.
    pub fn history_read(
        &self,
        name: &str,
        turn_seq: i64,
        offset: u64,
        limit: usize,
    ) -> Result<Value> {
        if !(4..=64 * 1024).contains(&limit) || offset > i64::MAX as u64 {
            return fail("invalid_history_page");
        }
        let head: Option<i64> = self
            .conn
            .prepare_cached("SELECT head FROM bots WHERE name=?")?
            .query_row([name], |r| r.get(0))
            .optional()?
            .ok_or(Error::new("bot_not_found"))?;
        // Walk only this bot's ancestry once. Crossing a turn start moves the
        // end boundary to its parent; by the requested start, both boundaries
        // are known, including for a fork cut midway through a turn.
        let boundary: Option<(i64, i64)> = self
            .conn
            .prepare_cached(
                "WITH RECURSIVE chain(id,parent,depth,turn_seq,end_id) AS (
                SELECT id,parent,depth,turn_seq,id FROM nodes WHERE id=?1
                UNION ALL SELECT n.id,n.parent,n.depth,n.turn_seq,
                    CASE WHEN c.turn_seq IS NOT NULL THEN c.parent ELSE c.end_id END
                FROM nodes n JOIN chain c ON n.id=c.parent
                WHERE c.turn_seq IS NULL OR c.turn_seq>?2)
             SELECT depth,end_id FROM chain WHERE turn_seq=?2 LIMIT 1",
            )?
            .query_row(params![head, turn_seq], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        let Some((depth, end)) = boundary else {
            return fail("turn_not_in_history");
        };
        let mut statement = self.conn.prepare_cached(
            "WITH RECURSIVE chain(id,parent,depth) AS (
                SELECT id,parent,depth FROM nodes WHERE id=?1
                UNION ALL SELECT n.id,n.parent,n.depth FROM nodes n JOIN chain c ON n.id=c.parent WHERE c.depth>?2)
             SELECT id FROM chain ORDER BY depth",
        )?;
        let mut rows = statement.query(params![end, depth])?;
        // Transform one item at a time, outside the ordered ancestry query:
        // SQLite must not materialize a turn's projected bodies to sort them.
        // Both length and slicing operate on BLOBs so offsets count UTF-8 bytes.
        // Compact provider whitespace in this view only: each item must occupy
        // one JSONL record even when its original response spans multiple lines.
        // Already single-line items need no rewrite (escaped newlines are fine).
        const READING_ITEM: &str = "CASE
            WHEN json_extract(CAST(item AS TEXT),'$.type')='reasoning'
             AND json_type(CAST(item AS TEXT),'$.encrypted_content') IS NOT NULL
            THEN CAST(json_remove(CAST(item AS TEXT),'$.encrypted_content') AS BLOB)
            WHEN instr(item,x'0a')=0 AND instr(item,x'0d')=0 THEN item
            ELSE CAST(json(CAST(item AS TEXT)) AS BLOB) END";
        let mut length = self.conn.prepare_cached(&format!(
            "SELECT length({READING_ITEM}) FROM nodes WHERE id=?"
        ))?;
        let mut slice = self.conn.prepare_cached(&format!(
            "SELECT substr({READING_ITEM},?,?) FROM nodes WHERE id=?"
        ))?;
        let mut text = String::new();
        let mut position = 0_u64;
        let mut items = 0;
        let mut done = true;
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let bytes = length.query_row([id], |r| r.get::<_, i64>(0))? as u64;
            let end = position + bytes + 1; // Include the JSONL newline.
            if offset >= end {
                position = end;
                continue;
            }
            if text.len() == limit {
                done = false;
                break;
            }
            let within = offset.saturating_sub(position);
            if within < bytes {
                let take = (bytes - within).min((limit - text.len()) as u64);
                let chunk: Vec<u8> =
                    slice.query_row(params![(within + 1) as i64, take as i64, id], |r| r.get(0))?;
                let piece = match std::str::from_utf8(&chunk) {
                    Ok(piece) => piece,
                    Err(error) if error.error_len().is_none() => {
                        std::str::from_utf8(&chunk[..error.valid_up_to()]).unwrap()
                    }
                    Err(_) => return fail("invalid_history_page"),
                };
                text.push_str(piece);
                if within + (piece.len() as u64) < bytes {
                    done = false;
                    break;
                }
            }
            if text.len() == limit {
                done = false; // The record's newline remains unread.
                break;
            }
            text.push('\n');
            items += 1;
            position = end;
        }
        if done && offset > position {
            return fail("invalid_history_page");
        }
        let next = offset + text.len() as u64;
        Ok(
            json!({"turn":turn_seq,"format":"jsonl","offset":offset,"next_offset":next,
            "items":items,"truncated":!done,"done":done,"text":text}),
        )
    }

    pub fn begin(
        &mut self,
        name: &str,
        request_id: &str,
        prompt: &str,
        capacity: bool,
        options: &TurnOptions,
    ) -> Result<Started> {
        let prior: Option<(i64, String, TurnOptions)> = self
            .conn
            .query_row(
                "SELECT id,prompt,workspace,model FROM turns WHERE bot=? AND request_id=?",
                params![name, request_id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        TurnOptions {
                            workspace: r.get(2)?,
                            model: r.get(3)?,
                        },
                    ))
                },
            )
            .optional()?;
        if let Some((turn, saved, saved_options)) = prior {
            if saved != prompt || saved_options != *options {
                return fail("idempotency_conflict");
            }
            return Ok(Started {
                turn,
                fresh: false,
                entry: None,
            });
        }
        // Admission applies only to new work, before any durable mutation.
        if !capacity {
            return fail("active_agent_limit");
        }
        let bot = self.inspect(name)?;
        if bot.running_turn.is_some() {
            return fail("bot_busy");
        }
        if bot.status == "uncertain" {
            return fail("tool_outcome_uncertain");
        }
        if bot.budget_tokens.is_some_and(|b| bot.tokens_used >= b) {
            return fail("budget_exhausted");
        }
        let workspace = options
            .workspace
            .as_deref()
            .or(bot.workspace.as_deref())
            .ok_or(Error::new("workspace_required"))?
            .to_owned();
        let item = bot.family()?.user_item(prompt)?;
        let tx = self.conn.transaction()?;
        // A deleted bot must never make an old turn handle refer to new work.
        let turn: i64 = tx
            .prepare_cached(
                "UPDATE turn_sequence SET last_id=last_id+1 WHERE singleton=1 RETURNING last_id",
            )?
            .query_row([], |r| r.get(0))?;
        tx.prepare_cached(
            "INSERT INTO turns(id,bot,request_id,prompt,status,workspace,model,started_ms) VALUES (?,?,?,?,'running',?,?,?)",
        )?.execute(
            params![turn, name, request_id, prompt, options.workspace, options.model, epoch_ms()],
        )?;
        tx.prepare_cached("INSERT INTO retained_turns(turn,bot) VALUES (?,?)")?
            .execute(params![turn, name])?;
        let head = node_with_turn(&tx, bot.head, &item, Some(turn))?;
        tx.execute(
            "UPDATE bots SET head=?,status='running',running_turn=? WHERE name=?",
            params![head, turn, name],
        )?;
        let data = json!({"request_id":request_id,"node":head,
            "workspace":workspace,
            "model":options.model.clone().unwrap_or_else(|| format!("{}/{}", bot.provider, bot.model))});
        let cursor = event(&tx, name, Some(turn), "accepted", data.clone())?;
        tx.commit()?;
        Ok(Started {
            turn,
            fresh: true,
            entry: Some(entry(cursor, name, Some(turn), "accepted", data)),
        })
    }
    pub fn append(
        &mut self,
        turn: i64,
        items: Vec<Bytes>,
        calls: &[ToolCall],
        usage: Option<&Usage>,
    ) -> Result<Vec<Value>> {
        let bot = self.active(turn)?;
        let tx = self.conn.transaction()?;
        let mut head = bot.head;
        let mut entries = Vec::new();
        for item in items {
            let id = node(&tx, head, &item)?;
            head = Some(id);
            let data = json!({"node":id});
            let cursor = event(&tx, &bot.name, Some(turn), "message", data.clone())?;
            entries.push(entry(cursor, &bot.name, Some(turn), "message", data));
        }
        if let Some(usage) = usage {
            entries.push(record_usage(&tx, &bot.name, turn, usage)?);
        }
        for call in calls {
            tx.execute(
                "INSERT INTO tools VALUES (?,?,'planned')",
                params![turn, call.call_id],
            )?;
        }
        tx.execute(
            "UPDATE bots SET head=? WHERE name=?",
            params![head, bot.name],
        )?;
        // A successful response consumes a round in the same durable commit
        // as its messages and tool plans. Failed requests end the turn.
        tx.execute(
            "UPDATE turns SET model_rounds=model_rounds+1 WHERE id=?",
            [turn],
        )?;
        tx.commit()?;
        Ok(entries)
    }
    /// Charge an unsuccessful provider call without accepting its output.
    pub fn failed_usage(&mut self, turn: i64, usage: &Usage) -> Result<Value> {
        let bot = self.active(turn)?;
        let tx = self.conn.transaction()?;
        let entry = record_usage(&tx, &bot.name, turn, usage)?;
        tx.execute(
            "UPDATE turns SET model_rounds=model_rounds+1 WHERE id=?",
            [turn],
        )?;
        tx.commit()?;
        Ok(entry)
    }
    pub fn tool_start(&mut self, turn: i64, call: &ToolCall) -> Result<Value> {
        let bot = self.active(turn)?;
        let tx = self.conn.transaction()?;
        if tx.execute(
            "UPDATE tools SET status='executing' WHERE turn=? AND call_id=? AND status='planned'",
            params![turn, call.call_id],
        )? != 1
        {
            return fail("invalid_tool_state");
        }
        let preview: String = call.arguments.chars().take(2048).collect();
        let data = json!({"call_id":call.call_id,"name":call.name,"arguments":preview,
            "arguments_truncated":preview.len() < call.arguments.len()});
        let cursor = event(&tx, &bot.name, Some(turn), "tool_started", data.clone())?;
        tx.commit()?;
        Ok(entry(cursor, &bot.name, Some(turn), "tool_started", data))
    }
    pub fn tool_finish(
        &mut self,
        turn: i64,
        call_id: &str,
        outcome: &Outcome,
    ) -> Result<(Bytes, Value)> {
        let bot = self.active(turn)?;
        let item = bot.family()?.tool_result_item(call_id, &outcome.output)?;
        let tx = self.conn.transaction()?;
        if tx.execute(
            "UPDATE tools SET status='completed' WHERE turn=? AND call_id=? AND status='executing'",
            params![turn, call_id],
        )? != 1
        {
            return fail("invalid_tool_state");
        }
        for (stream, data) in &outcome.artifacts {
            tx.execute(
                "INSERT INTO artifacts VALUES (?,?,?,?)",
                params![turn, call_id, stream, data],
            )?;
        }
        let head = node(&tx, bot.head, &item)?;
        tx.execute(
            "UPDATE bots SET head=? WHERE name=?",
            params![head, bot.name],
        )?;
        let artifacts: Vec<&str> = outcome.artifacts.iter().map(|(s, _)| *s).collect();
        let data = json!({"call_id":call_id,"node":head,"artifacts":artifacts});
        let cursor = event(&tx, &bot.name, Some(turn), "tool_completed", data.clone())?;
        tx.commit()?;
        Ok((
            item.into(),
            entry(cursor, &bot.name, Some(turn), "tool_completed", data),
        ))
    }
    /// The workspace and model reference a running turn must use.
    pub fn context(&self, turn: i64) -> Result<TurnContext> {
        let (workspace, model, model_rounds): (Option<String>, Option<String>, usize) =
            self.conn.query_row(
                "SELECT workspace,model,model_rounds FROM turns WHERE id=?",
                [turn],
                |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, u32>(2)? as usize)),
            )?;
        let bot = self.active(turn)?;
        Ok(TurnContext {
            model_rounds,
            workspace: workspace
                .or(bot.workspace)
                .ok_or(Error::new("workspace_required"))?,
            model: model.unwrap_or_else(|| format!("{}/{}", bot.provider, bot.model)),
            bot: bot.name,
        })
    }
    fn active(&self, turn: i64) -> Result<Bot> {
        let name: String =
            self.conn
                .query_row("SELECT bot FROM turns WHERE id=?", [turn], |r| r.get(0))?;
        let bot = self.inspect(&name)?;
        if bot.running_turn != Some(turn) {
            return fail("stale_turn");
        }
        Ok(bot)
    }
    /// End a turn and return every committed event in cursor order.
    pub fn finish(&mut self, turn: i64, error: Option<&Error>) -> Result<Vec<Value>> {
        let bot = self.active(turn)?;
        let waiting = self.waiting(turn)?;
        let tx = self.conn.transaction()?;
        let mut head = bot.head;
        let mut entries = Vec::new();
        if waiting.is_some() {
            // A parked turn's wait, and the planned calls behind it, never had
            // an external effect. Answer them so the conversation stays valid
            // for continuation, instead of leaving the bot uncertain.
            let unanswered: Vec<String> = tx
                .prepare("SELECT call_id FROM tools WHERE turn=? AND status IN ('planned','executing') ORDER BY rowid")?
                .query_map([turn], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            let family = bot.family()?;
            for call_id in unanswered {
                let output = json!({"error":"cancelled","detail":"turn interrupted while parked"})
                    .to_string();
                let item = family.tool_result_item(&call_id, &output)?;
                let id = node(&tx, head, &item)?;
                head = Some(id);
                tx.execute(
                    "UPDATE tools SET status='completed' WHERE turn=? AND call_id=?",
                    params![turn, call_id],
                )?;
                let data = json!({"call_id":call_id,"node":id,"artifacts":[],"cancelled":true});
                let cursor = event(&tx, &bot.name, Some(turn), "tool_completed", data.clone())?;
                entries.push(entry(cursor, &bot.name, Some(turn), "tool_completed", data));
            }
            tx.execute("UPDATE turns SET waiting=NULL WHERE id=?", [turn])?;
        }
        let pending: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM tools WHERE turn=? AND status IN ('planned','executing'))",
            [turn],
            |r| r.get(0),
        )?;
        let code = error.map(|e| e.code.as_str());
        let status = if pending {
            "uncertain"
        } else if matches!(code, Some("process_interrupted" | "cancelled")) {
            "interrupted"
        } else if code.is_some() {
            "failed"
        } else {
            "completed"
        };
        tx.execute(
            "UPDATE turns SET status=?,finished_ms=? WHERE id=?",
            params![status, epoch_ms(), turn],
        )?;
        tx.execute(
            "UPDATE bots SET head=?,running_turn=NULL,status=? WHERE name=?",
            params![head, status, bot.name],
        )?;
        if status == "completed" {
            tx.execute(
                "INSERT INTO checkpoints VALUES (?,?)",
                params![bot.name, head],
            )?;
        }
        let data = json!({"status":status,"checkpoint":if status == "completed" { head } else { None },
            "error":code,"detail":error.and_then(|e| e.detail.clone())});
        let cursor = event(&tx, &bot.name, Some(turn), "turn_finished", data.clone())?;
        tx.commit()?;
        entries.push(entry(cursor, &bot.name, Some(turn), "turn_finished", data));
        Ok(entries)
    }
    fn waiting(&self, turn: i64) -> Result<Option<Waiting>> {
        let row: Option<(String, Option<String>)> = self
            .conn
            .query_row("SELECT bot,waiting FROM turns WHERE id=?", [turn], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        let Some((bot, Some(encoded))) = row else {
            return Ok(None);
        };
        let mut waiting: Waiting = serde_json::from_str(&encoded)?;
        waiting.turn = turn;
        waiting.bot = bot;
        Ok(Some(waiting))
    }
    /// Park a running turn on handles while its wait call is executing.
    pub fn suspend(
        &mut self,
        turn: i64,
        call_id: &str,
        handles: &[String],
        deadline_ms: Option<u64>,
        pending: &[ToolCall],
    ) -> Result<Value> {
        let bot = self.active(turn)?;
        let executing: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM tools WHERE turn=? AND call_id=? AND status='executing')",
            params![turn, call_id],
            |r| r.get(0),
        )?;
        if !executing || bot.status != "running" {
            return fail("invalid_tool_state");
        }
        let waiting = Waiting {
            turn,
            bot: bot.name.clone(),
            call_id: call_id.into(),
            handles: handles.to_vec(),
            deadline_ms,
            pending: pending.to_vec(),
        };
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE turns SET status='waiting',waiting=? WHERE id=?",
            params![serde_json::to_string(&waiting)?, turn],
        )?;
        tx.execute("UPDATE bots SET status='waiting' WHERE name=?", [&bot.name])?;
        let data = json!({"call_id":call_id,"handles":handles,"deadline_ms":deadline_ms});
        let cursor = event(&tx, &bot.name, Some(turn), "turn_waiting", data.clone())?;
        tx.commit()?;
        Ok(entry(cursor, &bot.name, Some(turn), "turn_waiting", data))
    }
    /// Every parked turn, for re-registration after a restart.
    pub fn waiting_turns(&self) -> Result<Vec<Waiting>> {
        let mut statement = self
            .conn
            .prepare("SELECT id FROM turns WHERE status='waiting' ORDER BY id")?;
        let ids = statement
            .query_map([], |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.into_iter()
            .filter_map(|id| self.waiting(id).transpose())
            .collect()
    }
    /// A queued wake-up is valid only for the bot's current parked turn.
    /// Deleted bots and replaced turns are stale, not store failures.
    pub fn can_resume(&self, name: &str, turn: i64) -> Result<bool> {
        Ok(self
            .conn
            .prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM bots
                 WHERE name=? AND status='waiting' AND running_turn=?)",
            )?
            .query_row(params![name, turn], |r| r.get(0))?)
    }

    /// Bring a parked turn back to running; the caller then records the wait result.
    pub fn resume(&mut self, turn: i64) -> Result<(Waiting, Value)> {
        let waiting = self.waiting(turn)?.ok_or(Error::new("turn_not_waiting"))?;
        let bot = self.active(turn)?;
        if bot.status != "waiting" {
            return fail("turn_not_waiting");
        }
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE turns SET status='running',waiting=NULL WHERE id=?",
            [turn],
        )?;
        tx.execute("UPDATE bots SET status='running' WHERE name=?", [&bot.name])?;
        let data = json!({"call_id":waiting.call_id});
        let cursor = event(&tx, &bot.name, Some(turn), "turn_resumed", data.clone())?;
        tx.commit()?;
        Ok((
            waiting,
            entry(cursor, &bot.name, Some(turn), "turn_resumed", data),
        ))
    }
    /// A finished turn's outcome for a waiter: terminal status, error, and the
    /// final assistant text, bounded. `None` while the turn is still going.
    pub fn turn_outcome(&self, name: &str, turn: i64) -> Result<Option<Value>> {
        let owner: Option<(String, String)> = self
            .conn
            .query_row("SELECT bot,status FROM turns WHERE id=?", [turn], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        let Some((owner, status)) = owner else {
            return fail("turn_not_found");
        };
        if owner != name {
            return fail("turn_not_found");
        }
        if status == "running" || status == "waiting" {
            return Ok(None);
        }
        let finished: Option<String> = self
            .conn
            .query_row(
                "SELECT data FROM events WHERE turn=? AND kind='turn_finished' ORDER BY id DESC LIMIT 1",
                [turn],
                |r| r.get(0),
            )
            .optional()?;
        let mut outcome: Value = match finished {
            Some(data) => serde_json::from_str(&data)?,
            None => return fail("turn_result_pruned"),
        };
        let last: Option<i64> = self
            .conn
            .query_row(
                "SELECT data FROM events WHERE turn=? AND kind='message' ORDER BY id DESC LIMIT 1",
                [turn],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .and_then(|data| serde_json::from_str::<Value>(&data).ok())
            .and_then(|data| data["node"].as_i64());
        let text = match last {
            Some(node) => {
                let item: Vec<u8> =
                    self.conn
                        .query_row("SELECT item FROM nodes WHERE id=?", [node], |r| r.get(0))?;
                let item: Value = serde_json::from_slice(&item)?;
                assistant_text(&item)
            }
            None => String::new(),
        };
        let bounded: String = text.chars().take(16 * 1024).collect();
        outcome["text_truncated"] = json!(bounded.len() < text.len());
        outcome["text"] = json!(bounded);
        outcome["turn"] = json!(turn);
        Ok(Some(outcome))
    }
    /// Register a background command. Ids are unique for the store's lifetime,
    /// so a handle never resolves to a later command after a restart.
    pub fn process_start(&mut self, turn: i64, call_id: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO processes(turn,call_id,status) VALUES (?,?,'running')",
            params![turn, call_id],
        )?;
        Ok(self.conn.last_insert_rowid())
    }
    /// The outcome and its overflow become visible in one transaction.
    pub fn process_finish(
        &mut self,
        id: i64,
        result: &Value,
        artifacts: &[(&str, Vec<u8>)],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        if tx.execute(
            "UPDATE processes SET status='finished',result=? WHERE id=? AND status='running'",
            params![result.to_string(), id],
        )? != 1
        {
            return fail("invalid_process_state");
        }
        if !artifacts.is_empty() {
            let (turn, call_id): (i64, String) =
                tx.query_row("SELECT turn,call_id FROM processes WHERE id=?", [id], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?;
            for (stream, data) in artifacts {
                tx.execute(
                    "INSERT INTO artifacts VALUES (?,?,?,?)",
                    params![turn, call_id, stream, data],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }
    pub fn running_processes(&self) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT count(*) FROM processes WHERE status='running'",
            [],
            |r| r.get(0),
        )?)
    }
    /// `None` for an unknown id; otherwise the status and any recorded result.
    pub fn process_result(&self, id: i64) -> Result<Option<(String, Option<Value>)>> {
        let row: Option<(String, Option<String>)> = self
            .conn
            .query_row(
                "SELECT status,result FROM processes WHERE id=?",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match row {
            None => Ok(None),
            Some((status, result)) => Ok(Some((
                status,
                result.map(|r| serde_json::from_str(&r)).transpose()?,
            ))),
        }
    }
    /// Check only the suffix after a known-valid checkpoint. Completed turns
    /// and previously validated forks already establish a closed prefix. Walk
    /// backward one item at a time: no transcript sorting or prefix loading.
    /// Even old checkpoints must not end on an unpaired reasoning item.
    fn validate_fork_point(&self, node: i64) -> Result<()> {
        let reasoning: bool = self.conn.query_row(
            "SELECT COALESCE(json_extract(item,'$.type')='reasoning',0) FROM nodes WHERE id=?",
            [node],
            |row| row.get(0),
        )?;
        if reasoning {
            return fail("fork_point_splits_reasoning");
        }
        let mut statement = self.conn.prepare(
            "SELECT parent, CASE WHEN EXISTS(SELECT 1 FROM checkpoints WHERE head=nodes.id)
             THEN NULL ELSE item END FROM nodes WHERE id=?",
        )?;
        let mut next = Some(node);
        let mut answered = std::collections::HashSet::new();
        while let Some(id) = next {
            let (parent, raw): (Option<i64>, Option<Vec<u8>>) =
                statement.query_row([id], |row| Ok((row.get(0)?, row.get(1)?)))?;
            let Some(raw) = raw else {
                return Ok(());
            };
            let item: Value = serde_json::from_slice(&raw)?;
            match item["type"].as_str() {
                Some("function_call") => {
                    if !answered.remove(item["call_id"].as_str().unwrap_or("")) {
                        return fail("fork_point_has_open_tool_calls");
                    }
                }
                Some("function_call_output") => {
                    answered.insert(item["call_id"].as_str().unwrap_or("").to_owned());
                }
                _ => {
                    for block in item["content"].as_array().into_iter().flatten().rev() {
                        match block["type"].as_str() {
                            Some("tool_use") => {
                                if !answered.remove(block["id"].as_str().unwrap_or("")) {
                                    return fail("fork_point_has_open_tool_calls");
                                }
                            }
                            Some("tool_result") => {
                                answered
                                    .insert(block["tool_use_id"].as_str().unwrap_or("").to_owned());
                            }
                            _ => {}
                        }
                    }
                }
            }
            next = parent;
        }
        Ok(())
    }
    /// Branch a new bot from any message in the source's history. Without a
    /// node, the source's current head is used and the source must be idle,
    /// since a live head is still moving. The point must leave no tool call
    /// unanswered; the source itself is never changed.
    pub fn fork(
        &mut self,
        source: &str,
        node: Option<i64>,
        name: &str,
        workspace: Option<&str>,
        budget_tokens: Option<u64>,
    ) -> Result<Bot> {
        let parent = self.inspect(source)?;
        let checkpoint = match node {
            Some(node) => {
                if !self.in_lineage(parent.head, node)? {
                    return fail("node_not_in_source_history");
                }
                Some(node)
            }
            None => {
                if parent.running_turn.is_some() {
                    return fail("bot_busy");
                }
                parent.head
            }
        };
        if let Some(node) = checkpoint {
            self.validate_fork_point(node)?;
        }
        if self.exists(name)? {
            return fail("bot_exists");
        }
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO bots VALUES (?,?,?,'idle',NULL,?,?,?,?,?,?,0,NULL,0)",
            params![
                name,
                checkpoint,
                workspace,
                parent.provider,
                parent.family,
                parent.model,
                parent.instructions,
                parent.reasoning,
                budget_tokens.map(|b| b as i64)
            ],
        )?;
        if let Some(node) = checkpoint {
            tx.execute("INSERT INTO checkpoints VALUES (?,?)", params![name, node])?;
        }
        event(
            &tx,
            name,
            None,
            "forked",
            json!({"source":source,"checkpoint":checkpoint,"node":checkpoint}),
        )?;
        tx.commit()?;
        self.inspect(name)
    }
    pub fn events(&self, name: &str, after: i64, limit: usize) -> Result<Value> {
        self.inspect(name)?;
        if after < 0 || !(1..=256).contains(&limit) {
            return fail("invalid_event_page");
        }
        let mut stmt = self.conn.prepare(
            "SELECT id,turn,kind,data FROM events WHERE bot=? AND id>? ORDER BY id LIMIT ?",
        )?;
        let mut rows = stmt.query(params![name, after, limit as i64])?;
        let mut events = Vec::new();
        let mut cursor = after;
        // Leave ample room for the response envelope and its caller-supplied ID.
        let mut bytes = 0;
        let byte_limit = crate::output::MAX_EVENT / 2;
        while let Some(row) = rows.next()? {
            let next: i64 = row.get(0)?;
            let data: String = row.get(3)?;
            let item = entry(
                next,
                name,
                row.get::<_, Option<i64>>(1)?,
                &row.get::<_, String>(2)?,
                serde_json::from_str::<Value>(&data)?,
            );
            let size = crate::output::encoded_len(&item)? + 1;
            if bytes + size > byte_limit {
                if events.is_empty() {
                    return fail("event_page_item_limit");
                }
                break;
            }
            bytes += size;
            cursor = next;
            events.push(item);
        }
        let pruned: i64 =
            self.conn
                .query_row("SELECT pruned_cursor FROM bots WHERE name=?", [name], |r| {
                    r.get(0)
                })?;
        let mut page = json!({"events":events,"next_cursor":cursor});
        if after < pruned {
            // The caller asked for events that retention removed; say so
            // instead of replaying a silent gap.
            page["pruned_before"] = json!(pruned);
        }
        Ok(page)
    }
    /// Remove an idle bot with everything only it owns: its turns, tool
    /// intents, processes, artifacts, events, checkpoints, and the history
    /// nodes no other bot's lineage reaches. Shared prefixes stay for forks.
    pub fn delete_bot(&mut self, name: &str) -> Result<Value> {
        let bot = self.inspect(name)?;
        if bot.running_turn.is_some() {
            return fail("bot_busy");
        }
        let running: bool = self
            .conn
            .prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM turns t JOIN processes p ON p.turn=t.id
             WHERE t.bot=? AND p.status='running')",
            )?
            .query_row([name], |r| r.get(0))?;
        if running {
            return fail("bot_busy");
        }
        let tx = self.conn.transaction()?;
        // Preserve identity before freeing the suffix, in the same transaction.
        // The head is the largest ID on this append-only lineage. Together
        // with surviving nodes, this floor covers every committed node ID.
        if let Some(head) = bot.head {
            tx.execute(
                "UPDATE node_sequence SET last_id=MAX(last_id,?) WHERE singleton=1",
                [head],
            )?;
        }
        let mut deleted = json!({"turns":0,"events":0,"nodes":0});
        for (table, key) in [
            ("artifacts", "turn"),
            ("processes", "turn"),
            ("tools", "turn"),
        ] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE {key} IN (SELECT id FROM turns WHERE bot=?)"),
                [name],
            )?;
        }
        deleted["events"] = json!(tx.execute("DELETE FROM events WHERE bot=?", [name])?);
        tx.execute("DELETE FROM checkpoints WHERE bot=?", [name])?;
        deleted["turns"] = json!(tx.execute("DELETE FROM turns WHERE bot=?", [name])?);
        tx.execute("DELETE FROM bots WHERE name=?", [name])?;
        // Walk back from the head, freeing nodes until one is still reached
        // by another bot: as a head, a saved context start, or a parent of
        // a surviving branch. A fork's own suffix is what it leaves behind.
        let mut node = bot.head;
        let mut freed = 0;
        while let Some(id) = node {
            let referenced: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM bots WHERE head=?1 OR context_start=?1)
                    OR EXISTS(SELECT 1 FROM nodes WHERE parent=?1)",
                [id],
                |r| r.get(0),
            )?;
            if referenced {
                break;
            }
            let parent: Option<i64> =
                tx.query_row("SELECT parent FROM nodes WHERE id=?", [id], |r| r.get(0))?;
            tx.execute("DELETE FROM nodes WHERE id=?", [id])?;
            freed += 1;
            node = parent;
        }
        deleted["nodes"] = json!(freed);
        tx.commit()?;
        Ok(deleted)
    }
    /// Keep the newest `keep_turns` turns' records and drop the rest of the
    /// bot's events, tool intents, processes, and artifacts. The transcript
    /// and the turn rows themselves stay: retention here bounds what replay
    /// and tool retrieval keep, never what the model said.
    pub fn prune(&mut self, name: &str, keep_turns: usize) -> Result<Value> {
        self.inspect(name)?;
        if keep_turns == 0 {
            return fail("invalid_retention");
        }
        let floor: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM turns WHERE bot=? ORDER BY id DESC LIMIT 1 OFFSET ?",
                params![name, (keep_turns - 1) as i64],
                |r| r.get(0),
            )
            .optional()?;
        let Some(floor) = floor else {
            return Ok(json!({"events":0,"pruned_cursor":Value::Null}));
        };
        let tx = self.conn.transaction()?;
        for table in ["artifacts", "processes", "tools"] {
            // The candidate index contains only this bot's unpruned turns, not
            // its entire history or operational records owned by other bots.
            // Background commands may outlive their launching turn; their
            // running row is required when the result commits and waiters wake.
            let finished = if table == "processes" {
                "AND status!='running'"
            } else {
                ""
            };
            tx.prepare_cached(&format!(
                "DELETE FROM {table} WHERE turn IN
                 (SELECT turn FROM retained_turns WHERE bot=?1 AND turn<?2)
                 {finished}"
            ))?
            .execute(params![name, floor])?;
        }
        let cursor: Option<i64> = tx.query_row(
            "SELECT MAX(id) FROM events WHERE bot=? AND turn<?",
            params![name, floor],
            |r| r.get(0),
        )?;
        let events = tx.execute(
            "DELETE FROM events WHERE bot=? AND turn<?",
            params![name, floor],
        )?;
        // Keep pending background results discoverable by later prunes, even
        // after their launching turn's events and tool records are gone.
        tx.prepare_cached(
            "DELETE FROM retained_turns WHERE bot=?1 AND turn<?2
             AND NOT EXISTS(SELECT 1 FROM processes WHERE turn=retained_turns.turn AND status='running')",
        )?
        .execute(params![name, floor])?;
        if let Some(cursor) = cursor {
            tx.execute(
                "UPDATE bots SET pruned_cursor=MAX(pruned_cursor,?) WHERE name=?",
                params![cursor, name],
            )?;
        }
        let pruned: i64 =
            tx.query_row("SELECT pruned_cursor FROM bots WHERE name=?", [name], |r| {
                r.get(0)
            })?;
        tx.commit()?;
        Ok(json!({"events":events,"pruned_cursor":pruned}))
    }
    pub fn item(&self, name: &str, wanted: i64) -> Result<Value> {
        let head = self.inspect(name)?.head;
        if !self.in_lineage(head, wanted)? {
            return fail("item_not_in_bot_history");
        }
        let item: Vec<u8> =
            self.conn
                .query_row("SELECT item FROM nodes WHERE id=?", [wanted], |r| r.get(0))?;
        Ok(serde_json::from_slice(&item)?)
    }
    /// A bot's turns in id order, paged by `after`, with accounting a program
    /// needs without replaying events.
    pub fn turns(&self, name: &str, after: i64, limit: usize) -> Result<Value> {
        self.inspect(name)?;
        if after < 0 || !(1..=256).contains(&limit) {
            return fail("invalid_turn_page");
        }
        let mut statement = self.conn.prepare(
            "SELECT t.id,t.request_id,t.status,COALESCE(t.workspace,b.workspace),
                    COALESCE(t.model,b.provider||'/'||b.model),t.input_tokens,t.output_tokens,
                    t.model_rounds,t.started_ms,t.finished_ms,substr(t.prompt,1,200),length(t.prompt)
             FROM turns t JOIN bots b ON b.name=t.bot WHERE t.bot=? AND t.id>? ORDER BY t.id LIMIT ?",
        )?;
        let mut rows = statement.query(params![name, after, (limit + 1) as i64])?;
        let mut turns = Vec::new();
        let mut more = false;
        while let Some(r) = rows.next()? {
            if turns.len() == limit {
                more = true;
                break;
            }
            let preview: String = r.get(10)?;
            turns.push(
                json!({"turn":r.get::<_, i64>(0)?,"request_id":r.get::<_, String>(1)?,
                "status":r.get::<_, String>(2)?,"workspace":r.get::<_, Option<String>>(3)?,
                "model":r.get::<_, String>(4)?,"input_tokens":r.get::<_, i64>(5)?,
                "output_tokens":r.get::<_, i64>(6)?,"model_rounds":r.get::<_, i64>(7)?,
                "started_ms":r.get::<_, Option<i64>>(8)?,"finished_ms":r.get::<_, Option<i64>>(9)?,
                "prompt_preview":preview,"prompt_bytes":r.get::<_, i64>(11)?}),
            );
        }
        let next = more.then(|| {
            turns
                .last()
                .map(|t| t["turn"].clone())
                .unwrap_or(Value::Null)
        });
        Ok(json!({"turns":turns,"next_after":next}))
    }
    /// Own outputs stay a direct indexed lookup. Inherited outputs must have
    /// their tool-result node in the selected branch, never a later source turn.
    fn authorize_artifact(&self, name: &str, turn: i64, call_id: &str) -> Result<()> {
        let owner: Option<String> = self
            .conn
            .query_row("SELECT bot FROM turns WHERE id=?", [turn], |r| r.get(0))
            .optional()?;
        if owner.as_deref() == Some(name) {
            return Ok(());
        }
        let head: Option<Option<i64>> = self
            .conn
            .query_row("SELECT head FROM bots WHERE name=?", [name], |r| r.get(0))
            .optional()?;
        let node: Option<i64> = self.conn.query_row(
            "SELECT json_extract(data,'$.node') FROM events WHERE turn=? AND kind='tool_completed'
             AND json_extract(data,'$.call_id')=? ORDER BY id DESC LIMIT 1",
            params![turn, call_id], |r| r.get(0)).optional()?;
        if let (Some(head), Some(node)) = (head, node)
            && self.in_lineage(head, node)?
        {
            return Ok(());
        }
        fail("turn_not_found")
    }
    /// A retained stream as text, for the model's own `read`.
    pub fn artifact_lines(
        &self,
        name: &str,
        turn: i64,
        call_id: &str,
        stream: &str,
        offset: usize,
        limit: usize,
    ) -> Result<String> {
        if offset == 0 || !(1..=5000).contains(&limit) {
            return fail("invalid_tool_arguments");
        }
        self.authorize_artifact(name, turn, call_id)?;
        let data: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT data FROM artifacts WHERE turn=? AND call_id=? AND stream=?",
                params![turn, call_id, stream],
                |r| r.get(0),
            )
            .optional()?;
        let data = data.ok_or(Error::new("artifact_not_found"))?;
        crate::tools::page_lines(&String::from_utf8_lossy(&data), offset, limit)
    }
    pub fn artifact(&self, name: &str, turn: i64, call_id: &str) -> Result<Value> {
        self.authorize_artifact(name, turn, call_id)?;
        let mut statement = self
            .conn
            .prepare("SELECT stream,data FROM artifacts WHERE turn=? AND call_id=?")?;
        let mut rows = statement.query(params![turn, call_id])?;
        let mut streams = serde_json::Map::new();
        while let Some(row) = rows.next()? {
            let data: Vec<u8> = row.get(1)?;
            streams.insert(
                row.get::<_, String>(0)?,
                Value::String(String::from_utf8_lossy(&data).into_owned()),
            );
        }
        if streams.is_empty() {
            return fail("artifact_not_found");
        }
        Ok(Value::Object(streams))
    }

    /// Byte-addressed UTF-8 pages. SQL slicing bounds the bytes returned to
    /// Rust instead of assembling every retained stream in one response.
    pub fn artifact_page(
        &self,
        name: &str,
        turn: i64,
        call_id: &str,
        stream: &str,
        offset: u64,
        limit: usize,
    ) -> Result<Value> {
        if !(4..=64 * 1024).contains(&limit) || offset > i64::MAX as u64 - 1 {
            return fail("invalid_artifact_page");
        }
        self.authorize_artifact(name, turn, call_id)?;
        let row: Option<(i64, Vec<u8>)> = self.conn.query_row(
            "SELECT length(data),substr(data,?,?) FROM artifacts WHERE turn=? AND call_id=? AND stream=?",
            params![offset as i64 + 1, limit as i64, turn, call_id, stream],
            |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        let (total, bytes) = row.ok_or(Error::new("artifact_not_found"))?;
        let total = u64::try_from(total).map_err(|_| Error::new("storage_error"))?;
        if offset > total {
            return fail("invalid_artifact_page");
        }
        let text = match std::str::from_utf8(&bytes) {
            Ok(text) => text,
            Err(error) if error.error_len().is_none() => {
                std::str::from_utf8(&bytes[..error.valid_up_to()])
                    .map_err(|_| Error::new("invalid_artifact_page"))?
            }
            Err(_) => return fail("invalid_artifact_page"),
        };
        let next = offset + text.len() as u64;
        Ok(json!({"stream":stream,"offset":offset,"text":text,
            "next_offset":next,"total_bytes":total,"done":next == total}))
    }
}

/// Text of an assistant item in either family encoding; other content is skipped.
fn assistant_text(item: &Value) -> String {
    let mut text = String::new();
    if let Some(content) = item["content"].as_array() {
        for part in content {
            if let Some(piece) = part["text"].as_str()
                && matches!(part["type"].as_str(), Some("output_text" | "text"))
            {
                text.push_str(piece);
            }
        }
    }
    text
}

fn record_usage(conn: &Connection, bot: &str, turn: i64, usage: &Usage) -> Result<Value> {
    let data = serde_json::to_value(usage)?;
    let cursor = event(conn, bot, Some(turn), "usage", data.clone())?;
    conn.execute(
        "UPDATE turns SET input_tokens=input_tokens+?,output_tokens=output_tokens+? WHERE id=?",
        params![usage.input_tokens as i64, usage.output_tokens as i64, turn],
    )?;
    conn.execute(
        "UPDATE bots SET tokens_used=tokens_used+? WHERE name=?",
        params![
            usage.input_tokens.saturating_add(usage.output_tokens) as i64,
            bot
        ],
    )?;
    Ok(entry(cursor, bot, Some(turn), "usage", data))
}

fn epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn event(conn: &Connection, bot: &str, turn: Option<i64>, kind: &str, data: Value) -> Result<i64> {
    conn.prepare_cached("INSERT INTO events(bot,turn,kind,data) VALUES (?,?,?,?)")?
        .execute(params![bot, turn, kind, data.to_string()])?;
    Ok(conn.last_insert_rowid())
}
/// Append an item. Stored history has no lifetime cap; the per-request
/// context window is bounded separately. `turn` marks a turn's first item
/// (its user prompt) and receives the next ordinal along the lineage.
fn node(conn: &Connection, parent: Option<i64>, item: &[u8]) -> Result<i64> {
    node_with_turn(conn, parent, item, None)
}
/// Bring an older versioned store forward to the current schema inside the
/// caller's transaction. Each step converts stored data once; the runtime has
/// no other knowledge of earlier formats.
fn migrate(conn: &Connection, from: i32) -> Result<()> {
    if from < 6 {
        return fail_with(
            "store_schema_unsupported",
            format!(
                "store schema {from} has no migration to {}",
                Database::SCHEMA
            ),
        );
    }
    if from < 7 {
        // 6 -> 7: turn ordinals for context windows and history reads. A
        // turn's first node is the one its `accepted` event named.
        conn.execute_batch(
            "ALTER TABLE nodes ADD COLUMN turn INTEGER;
             ALTER TABLE nodes ADD COLUMN turn_seq INTEGER;
             ALTER TABLE bots ADD COLUMN context_start INTEGER REFERENCES nodes(id);",
        )?;
        let mut statement =
            conn.prepare("SELECT turn,data FROM events WHERE kind='accepted' ORDER BY id")?;
        // Accepted events and their first nodes commit together through one
        // writer. Cursor order already places parents before descendants.
        let mut rows = statement.query([])?;
        let mut parent_query = conn.prepare("SELECT parent FROM nodes WHERE id=?")?;
        let mut update = conn.prepare("UPDATE nodes SET turn=?,turn_seq=? WHERE id=?")?;
        while let Some(row) = rows.next()? {
            let turn: Option<i64> = row.get(0)?;
            let data: Value = serde_json::from_str(&row.get::<_, String>(1)?)
                .map_err(|_| Error::new("store_migration_invalid_event"))?;
            let (Some(turn), Some(node)) = (turn, data["node"].as_i64()) else {
                return fail("store_migration_invalid_event");
            };
            let parent: Option<i64> = parent_query.query_row([node], |r| r.get(0))?;
            let seq = match parent {
                None => 1,
                Some(parent) => previous_turn_seq(conn, parent)?.unwrap_or(0) + 1,
            };
            update.execute(params![turn, seq, node])?;
        }
    }
    if from < 8 {
        // 7 -> 8: retention. The indexes are in the shared DDL.
        conn.execute_batch(
            "ALTER TABLE bots ADD COLUMN pruned_cursor INTEGER NOT NULL DEFAULT 0;",
        )?;
    }
    if from < 9 {
        // A persistent high-water mark avoids rebuilding the referenced turns
        // table. Include fork-retained node markers whose owner was deleted.
        conn.execute_batch(
            "CREATE TABLE turn_sequence(singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                last_id INTEGER NOT NULL CHECK(last_id>=0));
             INSERT INTO turn_sequence SELECT 1, MAX(
                COALESCE((SELECT MAX(id) FROM turns),0),
                COALESCE((SELECT MAX(turn) FROM nodes),0));",
        )?;
    }
    if from < 10 {
        // Persist an allocation floor without rebuilding the history table.
        conn.execute_batch(
            "CREATE TABLE node_sequence(singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                last_id INTEGER NOT NULL CHECK(last_id>=0));
             INSERT INTO node_sequence SELECT 1, COALESCE(MAX(id),0) FROM nodes;",
        )?;
    }
    if from < 11 {
        // Index only turns that still own operational records, without
        // rewriting durable turn rows or reviving already-pruned history.
        conn.execute_batch(
            "CREATE TABLE retained_turns(turn INTEGER PRIMARY KEY REFERENCES turns(id) ON DELETE CASCADE,
                bot TEXT NOT NULL REFERENCES bots(name));
             INSERT INTO retained_turns SELECT t.id,t.bot FROM turns t JOIN
                (SELECT turn FROM tools UNION SELECT turn FROM artifacts
                 UNION SELECT turn FROM processes
                 UNION SELECT turn FROM events WHERE turn IS NOT NULL) r ON r.turn=t.id;",
        )?;
    }
    Ok(())
}

/// The nearest turn ordinal at or above `node` along its lineage; the walk
/// stops at the first numbered node.
fn previous_turn_seq(conn: &Connection, node: i64) -> Result<Option<i64>> {
    Ok(conn
        .prepare_cached(
            "WITH RECURSIVE chain(id,parent,turn_seq) AS (
                SELECT id,parent,turn_seq FROM nodes WHERE id=?
                UNION ALL SELECT n.id,n.parent,n.turn_seq FROM nodes n JOIN chain c ON n.id=c.parent
                WHERE c.turn_seq IS NULL)
             SELECT turn_seq FROM chain WHERE turn_seq IS NOT NULL LIMIT 1",
        )?
        .query_row([node], |r| r.get(0))
        .optional()?)
}

fn node_with_turn(
    conn: &Connection,
    parent: Option<i64>,
    item: &[u8],
    turn: Option<i64>,
) -> Result<i64> {
    let (bytes, depth): (i64, i64) = match parent {
        Some(id) => conn
            .prepare_cached("SELECT total_bytes,depth FROM nodes WHERE id=?")?
            .query_row([id], |r| Ok((r.get(0)?, r.get(1)?)))?,
        None => (0, 0),
    };
    if bytes < 0 || depth < 0 {
        return fail("storage_error");
    }
    let turn_seq = match (turn, parent) {
        (None, _) => None,
        (Some(_), None) => Some(1),
        (Some(_), Some(parent)) => Some(previous_turn_seq(conn, parent)?.unwrap_or(0) + 1),
    };
    // Surviving nodes and the durable deletion floor jointly bound every ID
    // ever committed. Allocate atomically in the insert, avoiding a separate
    // counter write for every message. Callers already hold a transaction.
    conn.prepare_cached(
        "INSERT INTO nodes(id,parent,item,total_bytes,depth,turn,turn_seq)
         SELECT MAX(last_id,COALESCE((SELECT MAX(id) FROM nodes),0))+1,?,?,?,?,?,?
         FROM node_sequence WHERE singleton=1",
    )?
    .execute(params![
        parent,
        item,
        bytes + item.len() as i64,
        depth + 1,
        turn,
        turn_seq
    ])?;
    Ok(conn.last_insert_rowid())
}
