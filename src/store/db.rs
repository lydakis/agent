use crate::{
    Error, Result, fail,
    history::{History, MAX_HISTORY_BYTES, MAX_ITEMS},
    provider::ToolCall,
};
use bytes::Bytes;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Debug, Serialize)]
pub struct Bot {
    pub name: String,
    pub head: Option<i64>,
    pub workspace: String,
    pub status: String,
    pub running_turn: Option<i64>,
}
pub struct Started {
    pub turn: i64,
    pub fresh: bool,
}
pub struct Database {
    conn: Connection,
}

impl Database {
    pub fn initialize(conn: Connection, configuration: &str) -> Result<Self> {
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            PRAGMA foreign_keys=ON; PRAGMA cache_size=-2048;
            CREATE TABLE IF NOT EXISTS configuration(value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS nodes(id INTEGER PRIMARY KEY, parent INTEGER REFERENCES nodes(id),
                item BLOB NOT NULL, total_bytes INTEGER NOT NULL, depth INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS bots(name TEXT PRIMARY KEY, head INTEGER REFERENCES nodes(id),
                workspace TEXT NOT NULL, status TEXT NOT NULL, running_turn INTEGER);
            CREATE TABLE IF NOT EXISTS turns(id INTEGER PRIMARY KEY, bot TEXT NOT NULL REFERENCES bots(name),
                request_id TEXT NOT NULL, prompt TEXT NOT NULL, status TEXT NOT NULL, UNIQUE(bot,request_id));
            CREATE TABLE IF NOT EXISTS checkpoints(bot TEXT NOT NULL REFERENCES bots(name), head INTEGER NOT NULL REFERENCES nodes(id),
                PRIMARY KEY(bot,head));
            CREATE TABLE IF NOT EXISTS tools(turn INTEGER NOT NULL REFERENCES turns(id), call_id TEXT NOT NULL,
                status TEXT NOT NULL, PRIMARY KEY(turn,call_id));
            CREATE TABLE IF NOT EXISTS events(id INTEGER PRIMARY KEY AUTOINCREMENT, bot TEXT NOT NULL REFERENCES bots(name),
                turn INTEGER, kind TEXT NOT NULL, data TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS events_bot_cursor ON events(bot,id);")?;
        let saved: Option<String> = conn
            .query_row("SELECT value FROM configuration", [], |r| r.get(0))
            .optional()?;
        match saved {
            None => {
                conn.execute("INSERT INTO configuration VALUES (?)", [configuration])?;
            }
            Some(saved) if saved != configuration => return fail("store_configuration_mismatch"),
            _ => {}
        }
        let mut db = Self { conn };
        // A committed tool intent without a result is never automatically retried.
        let pending: Vec<i64> = db
            .conn
            .prepare("SELECT id FROM turns WHERE status='running'")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for turn in pending {
            db.finish(turn, Some("process_interrupted"))?;
        }
        Ok(db)
    }

    pub fn inspect(&self, name: &str) -> Result<Bot> {
        self.conn
            .query_row(
                "SELECT name,head,workspace,status,running_turn FROM bots WHERE name=?",
                [name],
                |r| {
                    Ok(Bot {
                        name: r.get(0)?,
                        head: r.get(1)?,
                        workspace: r.get(2)?,
                        status: r.get(3)?,
                        running_turn: r.get(4)?,
                    })
                },
            )
            .optional()?
            .ok_or(Error("bot_not_found".into()))
    }
    pub fn create(&mut self, name: &str, workspace: &str) -> Result<Bot> {
        if self
            .conn
            .query_row("SELECT 1 FROM bots WHERE name=?", [name], |_| Ok(()))
            .optional()?
            .is_some()
        {
            return fail("bot_exists");
        }
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO bots VALUES (?,NULL,?,'idle',NULL)",
            params![name, workspace],
        )?;
        event(&tx, name, None, "created", json!({}))?;
        tx.commit()?;
        self.inspect(name)
    }
    pub fn load(&self, name: &str) -> Result<History> {
        let mut next = self.inspect(name)?.head;
        let mut items = Vec::new();
        let mut bytes = 0;
        let mut statement = self
            .conn
            .prepare("SELECT parent,item FROM nodes WHERE id=?")?;
        while let Some(id) = next {
            let (parent, item): (Option<i64>, Vec<u8>) =
                statement.query_row([id], |r| Ok((r.get(0)?, r.get(1)?)))?;
            bytes += item.len();
            if bytes > MAX_HISTORY_BYTES || items.len() >= MAX_ITEMS {
                return fail("history_limit");
            }
            items.push(Bytes::from(item));
            next = parent;
        }
        let mut history = History::default();
        for item in items.into_iter().rev() {
            history.append(item)?;
        }
        Ok(history)
    }
    pub fn begin(
        &mut self,
        name: &str,
        request_id: &str,
        prompt: &str,
        capacity: bool,
    ) -> Result<Started> {
        let prior: Option<(i64, String)> = self
            .conn
            .query_row(
                "SELECT id,prompt FROM turns WHERE bot=? AND request_id=?",
                params![name, request_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((turn, saved)) = prior {
            if saved != prompt {
                return fail("idempotency_conflict");
            }
            return Ok(Started { turn, fresh: false });
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
        let item = serde_json::to_vec(
            &json!({"role":"user","content":[{"type":"input_text","text":prompt}]}),
        )?;
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO turns(bot,request_id,prompt,status) VALUES (?,?,?,'running')",
            params![name, request_id, prompt],
        )?;
        let turn = tx.last_insert_rowid();
        let head = node(&tx, bot.head, &item)?;
        tx.execute(
            "UPDATE bots SET head=?,status='running',running_turn=? WHERE name=?",
            params![head, turn, name],
        )?;
        event(
            &tx,
            name,
            Some(turn),
            "accepted",
            json!({"request_id":request_id,"node":head}),
        )?;
        tx.commit()?;
        Ok(Started { turn, fresh: true })
    }
    pub fn append(&mut self, turn: i64, items: Vec<Bytes>, calls: &[ToolCall]) -> Result<()> {
        let bot = self.active(turn)?;
        let tx = self.conn.transaction()?;
        let mut head = bot.head;
        for item in items {
            let id = node(&tx, head, &item)?;
            head = Some(id);
            event(&tx, &bot.name, Some(turn), "message", json!({"node":id}))?;
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
        tx.commit()?;
        Ok(())
    }
    pub fn tool_start(&mut self, turn: i64, call_id: &str, name: &str) -> Result<()> {
        let bot = self.active(turn)?;
        let tx = self.conn.transaction()?;
        if tx.execute(
            "UPDATE tools SET status='executing' WHERE turn=? AND call_id=? AND status='planned'",
            params![turn, call_id],
        )? != 1
        {
            return fail("invalid_tool_state");
        }
        event(
            &tx,
            &bot.name,
            Some(turn),
            "tool_started",
            json!({"call_id":call_id,"name":name}),
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn tool_finish(&mut self, turn: i64, call_id: &str, output: &str) -> Result<Bytes> {
        let bot = self.active(turn)?;
        let item = serde_json::to_vec(
            &json!({"type":"function_call_output","call_id":call_id,"output":output}),
        )?;
        let tx = self.conn.transaction()?;
        if tx.execute(
            "UPDATE tools SET status='completed' WHERE turn=? AND call_id=? AND status='executing'",
            params![turn, call_id],
        )? != 1
        {
            return fail("invalid_tool_state");
        }
        let head = node(&tx, bot.head, &item)?;
        tx.execute(
            "UPDATE bots SET head=? WHERE name=?",
            params![head, bot.name],
        )?;
        event(
            &tx,
            &bot.name,
            Some(turn),
            "tool_completed",
            json!({"call_id":call_id,"node":head}),
        )?;
        tx.commit()?;
        Ok(item.into())
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
    pub fn finish(&mut self, turn: i64, error: Option<&str>) -> Result<Value> {
        let bot = self.active(turn)?;
        let pending: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM tools WHERE turn=? AND status!='completed')",
            [turn],
            |r| r.get(0),
        )?;
        let status = if pending {
            "uncertain"
        } else if matches!(error, Some("process_interrupted" | "cancelled")) {
            "interrupted"
        } else if error.is_some() {
            "failed"
        } else {
            "completed"
        };
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE turns SET status=? WHERE id=?",
            params![status, turn],
        )?;
        tx.execute(
            "UPDATE bots SET running_turn=NULL,status=? WHERE name=?",
            params![status, bot.name],
        )?;
        if status == "completed" {
            tx.execute(
                "INSERT INTO checkpoints VALUES (?,?)",
                params![bot.name, bot.head],
            )?;
        }
        let data = json!({"status":status,"checkpoint":if status == "completed" { bot.head } else { None },"error":error});
        let cursor = event(&tx, &bot.name, Some(turn), "turn_finished", data.clone())?;
        tx.commit()?;
        Ok(json!({"event":"turn_finished","bot":bot.name,"turn":turn,"cursor":cursor,"data":data}))
    }
    pub fn fork(
        &mut self,
        source: &str,
        checkpoint: i64,
        name: &str,
        workspace: &str,
    ) -> Result<Bot> {
        let parent = self.inspect(source)?;
        if self
            .conn
            .query_row(
                "SELECT 1 FROM checkpoints WHERE head=? LIMIT 1",
                [checkpoint],
                |_| Ok(()),
            )
            .optional()?
            .is_none()
        {
            return fail("invalid_checkpoint");
        }
        let mut next = parent.head;
        let mut found = false;
        for _ in 0..MAX_ITEMS {
            match next {
                Some(id) if id == checkpoint => {
                    found = true;
                    break;
                }
                Some(id) => {
                    next =
                        self.conn
                            .query_row("SELECT parent FROM nodes WHERE id=?", [id], |r| r.get(0))?
                }
                None => break,
            }
        }
        if !found {
            return fail("checkpoint_not_in_source_history");
        }
        if self
            .conn
            .query_row("SELECT 1 FROM bots WHERE name=?", [name], |_| Ok(()))
            .optional()?
            .is_some()
        {
            return fail("bot_exists");
        }
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO bots VALUES (?,?,?,'idle',NULL)",
            params![name, checkpoint, workspace],
        )?;
        tx.execute(
            "INSERT INTO checkpoints VALUES (?,?)",
            params![name, checkpoint],
        )?;
        event(
            &tx,
            name,
            None,
            "forked",
            json!({"source":source,"checkpoint":checkpoint}),
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
            let entry = json!({"cursor":next,"bot":name,"turn":row.get::<_, Option<i64>>(1)?,
                "event":row.get::<_, String>(2)?,"data":serde_json::from_str::<Value>(&data)?});
            let size = crate::output::encoded_len(&entry)? + 1;
            if bytes + size > byte_limit {
                if events.is_empty() {
                    return fail("event_page_item_limit");
                }
                break;
            }
            bytes += size;
            cursor = next;
            events.push(entry);
        }
        Ok(json!({"events":events,"next_cursor":cursor}))
    }
    pub fn item(&self, name: &str, wanted: i64) -> Result<Value> {
        let mut next = self.inspect(name)?.head;
        for _ in 0..MAX_ITEMS {
            let Some(id) = next else { break };
            let (parent, item): (Option<i64>, Vec<u8>) =
                self.conn
                    .query_row("SELECT parent,item FROM nodes WHERE id=?", [id], |r| {
                        Ok((r.get(0)?, r.get(1)?))
                    })?;
            if id == wanted {
                return Ok(serde_json::from_slice(&item)?);
            }
            next = parent;
        }
        fail("item_not_in_bot_history")
    }
}

fn event(conn: &Connection, bot: &str, turn: Option<i64>, kind: &str, data: Value) -> Result<i64> {
    conn.execute(
        "INSERT INTO events(bot,turn,kind,data) VALUES (?,?,?,?)",
        params![bot, turn, kind, data.to_string()],
    )?;
    Ok(conn.last_insert_rowid())
}
fn node(conn: &Connection, parent: Option<i64>, item: &[u8]) -> Result<i64> {
    let (bytes, depth): (i64, i64) = match parent {
        Some(id) => conn.query_row(
            "SELECT total_bytes,depth FROM nodes WHERE id=?",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?,
        None => (0, 0),
    };
    if bytes < 0
        || depth < 0
        || bytes + item.len() as i64 > MAX_HISTORY_BYTES as i64
        || depth >= MAX_ITEMS as i64
    {
        return fail("history_limit");
    }
    conn.execute(
        "INSERT INTO nodes(parent,item,total_bytes,depth) VALUES (?,?,?,?)",
        params![parent, item, bytes + item.len() as i64, depth + 1],
    )?;
    Ok(conn.last_insert_rowid())
}
