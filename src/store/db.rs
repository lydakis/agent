use crate::{
    Error, Result,
    codec::Family,
    fail,
    history::{History, MAX_HISTORY_BYTES, MAX_ITEMS},
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
    #[serde(default)]
    pub pending: Vec<ToolCall>,
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
    pub fn initialize(conn: Connection, configuration: &str) -> Result<Self> {
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            PRAGMA foreign_keys=ON; PRAGMA cache_size=-2048;
            CREATE TABLE IF NOT EXISTS configuration(value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS nodes(id INTEGER PRIMARY KEY, parent INTEGER REFERENCES nodes(id),
                item BLOB NOT NULL, total_bytes INTEGER NOT NULL, depth INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS bots(name TEXT PRIMARY KEY, head INTEGER REFERENCES nodes(id),
                workspace TEXT, status TEXT NOT NULL, running_turn INTEGER,
                provider TEXT NOT NULL, family TEXT NOT NULL, model TEXT NOT NULL,
                instructions TEXT NOT NULL, reasoning TEXT);
            CREATE TABLE IF NOT EXISTS turns(id INTEGER PRIMARY KEY, bot TEXT NOT NULL REFERENCES bots(name),
                request_id TEXT NOT NULL, prompt TEXT NOT NULL, status TEXT NOT NULL,
                workspace TEXT, model TEXT, waiting TEXT, model_rounds INTEGER NOT NULL DEFAULT 0,
                UNIQUE(bot,request_id));
            CREATE TABLE IF NOT EXISTS checkpoints(bot TEXT NOT NULL REFERENCES bots(name), head INTEGER NOT NULL REFERENCES nodes(id),
                PRIMARY KEY(bot,head));
            CREATE TABLE IF NOT EXISTS tools(turn INTEGER NOT NULL REFERENCES turns(id), call_id TEXT NOT NULL,
                status TEXT NOT NULL, PRIMARY KEY(turn,call_id));
            CREATE TABLE IF NOT EXISTS processes(id INTEGER PRIMARY KEY AUTOINCREMENT, turn INTEGER NOT NULL REFERENCES turns(id),
                call_id TEXT NOT NULL, status TEXT NOT NULL, result TEXT);
            CREATE TABLE IF NOT EXISTS artifacts(turn INTEGER NOT NULL REFERENCES turns(id), call_id TEXT NOT NULL,
                stream TEXT NOT NULL, data BLOB NOT NULL, PRIMARY KEY(turn,call_id,stream));
            CREATE TABLE IF NOT EXISTS events(id INTEGER PRIMARY KEY AUTOINCREMENT, bot TEXT NOT NULL REFERENCES bots(name),
                turn INTEGER, kind TEXT NOT NULL, data TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS events_bot_cursor ON events(bot,id);
            CREATE INDEX IF NOT EXISTS events_turn_kind_cursor ON events(turn,kind,id);")?;
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
    const COLUMNS: &str =
        "name,head,workspace,status,running_turn,provider,family,model,instructions,reasoning";
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
            "SELECT name,head,workspace,status,running_turn,provider,family,model,reasoning
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
                "reasoning":r.get::<_, Option<String>>(8)?});
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
            "INSERT INTO bots VALUES (?,NULL,?,'idle',NULL,?,?,?,?,?)",
            params![
                name,
                workspace,
                binding.provider,
                binding.family.name(),
                binding.model,
                binding.instructions,
                binding.reasoning
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
    pub fn load(&self, name: &str) -> Result<History> {
        let head = self.inspect(name)?.head;
        let mut history = History::default();
        let Some(head) = head else {
            return Ok(history);
        };
        let mut statement = self.conn.prepare(
            "WITH RECURSIVE chain(id,parent,item,depth) AS (
                SELECT id,parent,item,depth FROM nodes WHERE id=?
                UNION ALL SELECT n.id,n.parent,n.item,n.depth FROM nodes n JOIN chain c ON n.id=c.parent)
             SELECT item FROM chain ORDER BY depth",
        )?;
        let mut rows = statement.query([head])?;
        let mut bytes = 0;
        while let Some(row) = rows.next()? {
            let item: Vec<u8> = row.get(0)?;
            bytes += item.len();
            if bytes > MAX_HISTORY_BYTES || history.len() >= MAX_ITEMS {
                return fail("history_limit");
            }
            history.append(Bytes::from(item))?;
        }
        Ok(history)
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
        let workspace = options
            .workspace
            .as_deref()
            .or(bot.workspace.as_deref())
            .ok_or(Error::new("workspace_required"))?
            .to_owned();
        let item = bot.family()?.user_item(prompt)?;
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO turns(bot,request_id,prompt,status,workspace,model) VALUES (?,?,?,'running',?,?)",
            params![name, request_id, prompt, options.workspace, options.model],
        )?;
        let turn = tx.last_insert_rowid();
        let head = node(&tx, bot.head, &item)?;
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
            let data = serde_json::to_value(usage)?;
            let cursor = event(&tx, &bot.name, Some(turn), "usage", data.clone())?;
            entries.push(entry(cursor, &bot.name, Some(turn), "usage", data));
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
            "UPDATE turns SET status=? WHERE id=?",
            params![status, turn],
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
            None => json!({"status":status}),
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
    pub fn fork(
        &mut self,
        source: &str,
        checkpoint: i64,
        name: &str,
        workspace: Option<&str>,
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
        if !self.in_lineage(parent.head, checkpoint)? {
            return fail("checkpoint_not_in_source_history");
        }
        if self.exists(name)? {
            return fail("bot_exists");
        }
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO bots VALUES (?,?,?,'idle',NULL,?,?,?,?,?)",
            params![
                name,
                checkpoint,
                workspace,
                parent.provider,
                parent.family,
                parent.model,
                parent.instructions,
                parent.reasoning
            ],
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
        Ok(json!({"events":events,"next_cursor":cursor}))
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
    pub fn artifact(&self, name: &str, turn: i64, call_id: &str) -> Result<Value> {
        let owner: Option<String> = self
            .conn
            .query_row("SELECT bot FROM turns WHERE id=?", [turn], |r| r.get(0))
            .optional()?;
        if owner.as_deref() != Some(name) {
            return fail("turn_not_found");
        }
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
        let owner: Option<String> = self
            .conn
            .query_row("SELECT bot FROM turns WHERE id=?", [turn], |r| r.get(0))
            .optional()?;
        if owner.as_deref() != Some(name) {
            return fail("turn_not_found");
        }
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
