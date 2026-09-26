use super::artifact;
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

// Sharing tiny prompts adds an index entry without avoiding an overflow page.
const PROMPT_SHARE_BYTES: usize = 4096;

const STEER_BATCH_ITEMS: usize = 32;
const STEER_BATCH_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct Bot {
    pub name: String,
    /// Store-wide identity, never reused after deletion. A name can be
    /// recycled; a retry that carries the id cannot land on the new holder.
    pub id: i64,
    pub head: Option<i64>,
    /// Lifetime cap on input plus output tokens; checked before each model call.
    pub budget_tokens: Option<u64>,
    pub tokens_used: u64,
    /// Input tokens sent and, of those, the ones the provider served from
    /// its prompt cache. Their ratio is what the context window's hysteresis
    /// exists to keep high.
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_hit: f64,
    /// Default directory for submissions that name none; a bot need not have one.
    pub workspace: Option<String>,
    pub status: String,
    pub running_turn: Option<i64>,
    pub provider: String,
    pub family: String,
    pub model: String,
    pub instructions: String,
    pub reasoning: Option<String>,
    /// The tools this bot may call, chosen at creation and kept with it.
    pub tools: Vec<String>,
    /// The bot whose client created or forked this one, as that client
    /// declared it (the CLI takes it from `AGENT_BOT`). Bots are peers;
    /// this is lineage for people, not authority.
    pub created_by: Option<String>,
    /// The creator's captured identity, validated by the store. A later bot
    /// reusing the name is not mistaken for it. `None` for a root bot.
    pub created_by_id: Option<i64>,
    /// The node of the bot's current carry-forward note, if it wrote one.
    pub note: Option<i64>,
    /// The node of the bot's current compaction, if any, and the client's
    /// compaction instructions and summarizer model; without instructions
    /// the bot never compacts.
    pub compaction: Option<i64>,
    pub compaction_instructions: Option<String>,
    pub compaction_model: Option<String>,
    /// The node of the bot's current elision version, if any: tool results
    /// through its floor go to the model as stubs.
    pub elision: Option<i64>,
    /// The bot whose provider prompt cache this one shares: a fork with its
    /// source's instructions starts from the source's cached prefix. `None`
    /// is the bot's own.
    pub cache_bot: Option<i64>,
    /// Anthropic thinking replay: a fingerprint of the context in front of
    /// the window at the last request, and the first node whose thinking
    /// was written under it. Blocks on older nodes were bound to a context
    /// the provider no longer sees, so requests send them without thinking.
    #[serde(skip)]
    pub thinking_prefix: Option<i64>,
    #[serde(skip)]
    pub thinking_floor: i64,
    /// Anthropic server-side fallbacks: a declined request is rerun on the
    /// model Anthropic recommends instead of failing the turn. The client's
    /// choice at creation; forks inherit it.
    pub fallbacks: bool,
}
impl Bot {
    /// The bot id that keys this bot's provider prompt cache.
    pub fn cache_bot(&self) -> i64 {
        self.cache_bot.unwrap_or(self.id)
    }
    pub fn family(&self) -> Result<Family> {
        Family::parse(&self.family).ok_or(Error::new("store_family_unsupported"))
    }
}
/// What a fork may choose for itself; everything else comes from the source.
#[derive(Default, Clone, Copy)]
pub struct Fork<'a> {
    /// A node id from the source's history; `None` is its current head.
    pub checkpoint: Option<i64>,
    pub workspace: Option<&'a str>,
    pub budget_tokens: Option<u64>,
    pub created_by: Option<&'a str>,
    pub created_by_id: Option<i64>,
}
/// Provider binding chosen at creation; immutable for the bot's lifetime.
pub struct Binding<'a> {
    pub provider: &'a str,
    pub family: Family,
    pub model: &'a str,
    pub instructions: &'a str,
    pub reasoning: Option<&'a str>,
    pub budget_tokens: Option<u64>,
    pub tools: &'a [String],
    pub created_by: Option<&'a str>,
    pub created_by_id: Option<i64>,
    /// Compaction instructions and an optional summarizer model, both the
    /// client's; with no instructions the bot never compacts.
    pub compaction_instructions: Option<&'a str>,
    pub compaction_model: Option<&'a str>,
    /// Anthropic server-side fallbacks for this bot.
    pub fallbacks: bool,
}
#[derive(Debug)]
pub struct Started {
    pub turn: i64,
    pub fresh: bool,
    /// `running`, `queued` behind the bot's own work, or `ready` for a slot.
    pub status: &'static str,
    /// The durable `accepted` or `queued` entry, present only for fresh submissions.
    pub entry: Option<Value>,
}
/// Per-turn overrides of the bot's defaults, recorded with the turn.
#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct TurnOptions {
    pub workspace: Option<String>,
    pub model: Option<String>,
    pub delivery: Delivery,
    /// A strict steer: for this running turn or nobody. Never absorbed by
    /// another turn, never started as new work; `stale_turn` instead.
    pub expected_turn: Option<i64>,
}
/// What a submission does when the bot is busy or the daemon is full.
/// `Reject` answers `bot_busy` or `active_agent_limit`. `Queue` records the
/// turn and starts it when the bot and a slot are free. `Steer` is a queued
/// turn the bot's running turn may absorb at its next round boundary.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    #[default]
    Reject,
    Queue,
    Steer,
}
impl Delivery {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "reject" => Some(Self::Reject),
            "queue" => Some(Self::Queue),
            "steer" => Some(Self::Steer),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Reject => "reject",
            Self::Queue => "queue",
            Self::Steer => "steer",
        }
    }
}
/// What the storage worker hands to the publisher after each job, in commit
/// order: every durable event committed past its watermark, then the
/// outcomes of turns that job ended, for their waiters.
#[derive(Debug)]
pub enum Publication {
    Event(Value),
    Finished {
        bot: String,
        turn: i64,
        outcome: Value,
    },
}
/// Steers a running turn absorbed at a round boundary: their user items are
/// on the lineage and their rows are finished as `steered`.
#[derive(Default)]
pub struct Absorbed {
    pub entries: Vec<Value>,
    /// Each steered turn with its outcome, for its waiters.
    pub outcomes: Vec<(i64, Value)>,
    /// Continue this boundary's finite snapshot in another bounded call.
    pub next_through: Option<i64>,
}
/// A turn parked on handles; it holds no task or memory until they resolve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct Waiting {
    pub turn: i64,
    pub bot: String,
    pub call_id: String,
    pub handles: Vec<String>,
    pub deadline_ms: Option<u64>,
    /// Resume on the first resolved handle rather than all of them.
    #[serde(default)]
    pub any: bool,
    /// Tool calls from the same model response that follow the wait.
    pub pending: Vec<ToolCall>,
    /// A rate-limit park's start time. Its deadline is only a wake-up hint;
    /// elapsed waiting is charged when resumed or finished, even after restart.
    #[serde(default)]
    pub paced_since_ms: Option<i64>,
    /// Retry state of the unfinished model call, separate from turn totals.
    #[serde(default)]
    pub call_attempts: u32,
    #[serde(default)]
    pub call_spent_ms: u64,
    /// Which model call to resume; ordinary calls bypass compaction once.
    #[serde(default)]
    pub compaction: bool,
    /// The provider's sticky-routing token for the turn, so its calls after
    /// the park keep going to the server that holds its cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
}
impl Waiting {
    fn paced_elapsed_ms(&self) -> i64 {
        self.paced_since_ms
            .map_or(0, |since| epoch_ms().saturating_sub(since).max(0))
    }
}
/// The bounded request context: ordered node ids and exact item bytes.
#[derive(Debug)]
pub struct Window {
    pub family: Family,
    pub ids: Vec<i64>,
    /// Each item's encoded length, so read-ahead can be bounded in bytes.
    pub sizes: Vec<u32>,
    /// Bytes each item loses when sent without its thinking blocks.
    pub thinking: Vec<u32>,
    /// Sum of item lengths, without separators.
    pub item_bytes: i64,
    /// Full unsummarized span, even when the bounded window omits a backlog.
    pub unsummarized: super::ContextUsage,
    pub omitted_items: i64,
    pub omitted_turns: i64,
    /// Whether the bot may call the history tool, so the context note names
    /// it only when it can.
    pub history: bool,
    /// The bot's carry-forward note: its version node and text.
    pub note: Option<(i64, String)>,
    /// The bot's current compaction, if any.
    pub compaction: Option<CompactionView>,
    /// Tool results with ids up to this one that have stubs go as their
    /// stubs, which `sizes` counts; zero when nothing is elided.
    pub elided: i64,
}
/// What a compaction left in place of the turns it covered.
#[derive(Debug, Clone)]
pub struct CompactionView {
    pub version: i64,
    pub summary: String,
    /// The covered turns' user prompts, verbatim within bounds: ordinal and text.
    pub prompts: Vec<(i64, String)>,
    pub covered: (i64, i64),
}
/// Where an elision moves a bot's floor: tool results with stubs through
/// this node go as their stubs, newly `results` of them, saving `saved_bytes`.
#[derive(Debug, Clone, Copy)]
pub struct ElisionPlan {
    pub through: i64,
    pub results: i64,
    pub saved_bytes: i64,
}
/// What a compaction has to summarize, chosen at a turn boundary.
#[derive(Debug, Clone)]
pub struct CompactionPlan {
    /// The prompt node the verbatim tail starts at: the new context start.
    pub cut: i64,
    /// The nodes to summarize, oldest first.
    pub ids: Vec<i64>,
    pub sizes: Vec<u32>,
    /// The covered turns' user prompts, bounded, oldest first.
    pub prompts: Vec<(i64, String)>,
    pub covered: (i64, i64),
    pub previous_summary: Option<String>,
    /// A bounded step through a backlog larger than the budget: the span
    /// ends where the next step starts, not at the verbatim tail.
    pub catch_up: bool,
    pub summary_bytes: usize,
    /// The elision floor the span is read under: results through it go to
    /// the summarizer as their stubs, which `sizes` counts.
    pub elided: i64,
}
impl CompactionPlan {
    /// The summarizer request around the span's items: the previous summary
    /// to merge, if any, with its separating comma, and the request to
    /// write, with its leading comma.
    pub fn frame(
        family: Family,
        previous_summary: Option<&str>,
        summary_bytes: usize,
    ) -> Result<(Vec<u8>, Vec<u8>)> {
        let mut head = Vec::new();
        if let Some(previous) = previous_summary {
            head = family.user_item(&format!(
                "[previous summary, to merge with the turns below]\n{previous}"
            ))?;
            head.push(b',');
        }
        let mut tail = family.user_item(&format!(
            "[compaction request] Write the summary of the conversation above now, following your instructions. Keep the summary within {summary_bytes} UTF-8 bytes; be shorter when possible."
        ))?;
        tail.insert(0, b',');
        Ok((head, tail))
    }
}
/// Where compaction planning stands: a plan, or a backlog larger than the
/// budget to walk first.
#[derive(Debug)]
pub enum Planning {
    Plan(CompactionPlan),
    CatchUp(CatchUp),
}
/// Oldest node, depth, own total, ordinal, child on the lineage and its
/// ordinal, and the bounded prompt text inside the budget.
type CatchUpRow = (
    i64,
    i64,
    i64,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<String>,
);
/// A catch-up walk from the head back to the previous cut. Only the rows
/// that start inside the budget are kept, so its memory is bounded by the
/// budget however long the backlog is.
#[derive(Debug)]
pub struct CatchUp {
    head: i64,
    previous_cut: i64,
    /// The head's bytes, and the bytes and depth before the previous cut,
    /// counted as sent.
    totals: (i64, i64, i64),
    /// The bot's elision floor and what its stubs save through it.
    elision: (i64, i64),
    /// Verbatim tail byte/item targets, then input byte/item limits.
    bounds: (i64, i64, i64, i64),
    /// Where the next piece starts, and the child it continues from.
    next: Option<(i64, Option<i64>, Option<i64>)>,
    /// Rows inside the budget, in no particular order.
    rows: Vec<CatchUpRow>,
}
impl CatchUp {
    pub fn done(&self) -> bool {
        self.next.is_none()
    }
}
/// Where and with which model a turn runs.
pub struct TurnContext {
    pub model_rounds: usize,
    pub bot: String,
    pub bot_id: i64,
    pub created_by: Option<String>,
    pub created_by_id: Option<i64>,
    pub workspace: String,
    pub model: String,
}
pub struct Database {
    conn: Connection,
    /// Bounds on submissions waiting to start, turns and prompt bytes; zero
    /// is unbounded. Daemon configuration, set once after open.
    pending_limits: (usize, usize),
    /// Queued and ready turns and their prompt bytes. Counted from the rows
    /// at open and kept by the one writer at each transition, so admission
    /// and `stats` cost nothing on the store.
    pending: (i64, i64),
    /// Outcomes of turns ended by the current job, published after its
    /// events. Captured inside the job, so retention in the same job
    /// cannot remove what a waiter is owed.
    outcomes: Vec<(String, i64, Value)>,
    /// A group was abandoned and `pending` not yet recounted from the rows;
    /// no job runs until a recount succeeds.
    pending_stale: bool,
}

/// The share of input tokens the provider served from its prompt cache,
/// to three places; zero when nothing was sent.
pub fn cache_hit(cached: i64, input: i64) -> f64 {
    if input <= 0 {
        return 0.0;
    }
    ((cached.max(0) as f64 / input as f64) * 1000.0).round() / 1000.0
}
fn split_tools(joined: &str) -> Vec<String> {
    joined
        .split(',')
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .collect()
}
/// Durable events and their live copies share one shape.
fn entry(cursor: i64, bot: &str, turn: Option<i64>, kind: &str, data: Value) -> Value {
    json!({"cursor":cursor,"bot":bot,"turn":turn,"event":kind,"data":data})
}

impl Database {
    /// Stored schema version, kept in `PRAGMA user_version`. Stores created
    /// before versioning and stores from newer binaries are rejected; an older
    /// versioned store is migrated forward, one version at a time, at open.
    pub const SCHEMA: i32 = 29;
    /// Verbatim user prompts a compaction keeps: per-prompt text, and the
    /// total text plus `(ordinal, String)` entry metadata. Empty entries cost
    /// space too, so the retained list cannot grow with conversation length.
    pub const COMPACTION_PROMPT_BYTES: usize = 2048;
    pub const COMPACTION_PROMPTS_BYTES: usize = 16 * 1024;
    /// Turns per retention piece: a delete or explicit prune of a large bot
    /// runs as a series of jobs this size, so other bots' work interleaves.
    /// Small, because every job of a turn in flight can land behind one
    /// piece; a piece of four turns' records runs in about two milliseconds.
    pub const RETENTION_PIECE: usize = 4;

    /// A second connection that only reads. The writer owns the file, its
    /// lock, migration, and recovery; this one sees a job's writes once that
    /// job is answered and never takes the write lock. It can still run the
    /// checkpoint SQLite makes when the last connection closes, so it syncs
    /// checkpoints the way the writer does.
    pub fn reader(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "PRAGMA query_only=ON; PRAGMA cache_size=-2048; PRAGMA checkpoint_fullfsync=ON;",
        )?;
        Ok(Self {
            conn,
            pending_limits: (0, 0),
            pending: (0, 0),
            outcomes: Vec::new(),
            pending_stale: false,
        })
    }

    pub fn initialize(conn: Connection) -> Result<Self> {
        // On macOS a plain fsync leaves writes in the drive's cache, so FULL
        // survives a power cut only with F_FULLFSYNC, which SQLite sends when
        // these are on. Elsewhere they change nothing.
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            PRAGMA fullfsync=ON; PRAGMA checkpoint_fullfsync=ON;
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
            CREATE TABLE IF NOT EXISTS nodes(id INTEGER PRIMARY KEY, parent INTEGER REFERENCES nodes(id),
                item BLOB NOT NULL, total_bytes INTEGER NOT NULL, depth INTEGER NOT NULL,
                turn INTEGER, turn_seq INTEGER, thinking INTEGER NOT NULL DEFAULT 0,
                elided INTEGER NOT NULL DEFAULT 0, total_elided INTEGER NOT NULL DEFAULT 0);
            CREATE INDEX IF NOT EXISTS nodes_turn_seq ON nodes(turn_seq) WHERE turn_seq IS NOT NULL;
            CREATE UNIQUE INDEX IF NOT EXISTS nodes_turn ON nodes(turn) WHERE turn IS NOT NULL;
            CREATE TABLE IF NOT EXISTS notes(node INTEGER PRIMARY KEY REFERENCES nodes(id),
                previous INTEGER REFERENCES notes(node), text TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS notes_previous ON notes(previous);
            CREATE TABLE IF NOT EXISTS compactions(node INTEGER PRIMARY KEY REFERENCES nodes(id),
                previous INTEGER REFERENCES compactions(node), cut INTEGER NOT NULL REFERENCES nodes(id),
                summary TEXT NOT NULL, prompts TEXT NOT NULL,
                covered_from INTEGER NOT NULL, covered_to INTEGER NOT NULL);
            CREATE INDEX IF NOT EXISTS compactions_previous ON compactions(previous);
            CREATE INDEX IF NOT EXISTS compactions_cut ON compactions(cut);
            CREATE TABLE IF NOT EXISTS stubs(node INTEGER PRIMARY KEY REFERENCES nodes(id),
                item BLOB NOT NULL);
            CREATE TABLE IF NOT EXISTS elisions(node INTEGER PRIMARY KEY REFERENCES nodes(id),
                previous INTEGER REFERENCES elisions(node), through INTEGER NOT NULL,
                saved INTEGER NOT NULL);
            CREATE INDEX IF NOT EXISTS elisions_previous ON elisions(previous);
            CREATE TABLE IF NOT EXISTS node_sequence(singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                last_id INTEGER NOT NULL CHECK(last_id>=0));
            INSERT OR IGNORE INTO node_sequence VALUES (1,0);
            CREATE TABLE IF NOT EXISTS bots(name TEXT PRIMARY KEY, head INTEGER REFERENCES nodes(id),
                id INTEGER NOT NULL,
                workspace TEXT, status TEXT NOT NULL, running_turn INTEGER,
                provider TEXT NOT NULL, family TEXT NOT NULL, model TEXT NOT NULL,
                instructions TEXT NOT NULL, reasoning TEXT,
                budget_tokens INTEGER, tokens_used INTEGER NOT NULL DEFAULT 0,
                context_start INTEGER REFERENCES nodes(id),
                pruned_cursor INTEGER NOT NULL DEFAULT 0,
                tools TEXT NOT NULL DEFAULT 'shell,read,write,edit,wait,history',
                input_tokens INTEGER NOT NULL DEFAULT 0,
                cached_input_tokens INTEGER NOT NULL DEFAULT 0,
                created_by TEXT,
                created_by_id INTEGER,
                note INTEGER REFERENCES notes(node),
                compaction INTEGER REFERENCES compactions(node),
                compaction_instructions TEXT, compaction_model TEXT,
                cache_bot INTEGER, thinking_prefix INTEGER,
                thinking_floor INTEGER NOT NULL DEFAULT 0,
                fallbacks INTEGER NOT NULL DEFAULT 0,
                elision INTEGER REFERENCES elisions(node));
            CREATE UNIQUE INDEX IF NOT EXISTS bots_id ON bots(id);
            CREATE INDEX IF NOT EXISTS bots_note ON bots(note);
            CREATE INDEX IF NOT EXISTS bots_compaction ON bots(compaction);
            CREATE INDEX IF NOT EXISTS bots_elision ON bots(elision);
            CREATE TABLE IF NOT EXISTS bot_sequence(singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                last_id INTEGER NOT NULL CHECK(last_id>=0));
            INSERT OR IGNORE INTO bot_sequence VALUES (1,0);
            CREATE TABLE IF NOT EXISTS store(singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                identity INTEGER NOT NULL);
            INSERT OR IGNORE INTO store VALUES (1,random());
            CREATE INDEX IF NOT EXISTS nodes_parent ON nodes(parent);
            CREATE INDEX IF NOT EXISTS bots_head ON bots(head);
            CREATE INDEX IF NOT EXISTS bots_context_start ON bots(context_start);
            CREATE TABLE IF NOT EXISTS turns(id INTEGER PRIMARY KEY, bot TEXT NOT NULL REFERENCES bots(name),
                request_id TEXT NOT NULL, prompt TEXT NOT NULL, status TEXT NOT NULL,
                prompt_node INTEGER REFERENCES nodes(id),
                workspace TEXT, model TEXT, waiting TEXT, model_rounds INTEGER NOT NULL DEFAULT 0,
                input_tokens INTEGER NOT NULL DEFAULT 0, output_tokens INTEGER NOT NULL DEFAULT 0,
                started_ms INTEGER, finished_ms INTEGER,
                retries INTEGER NOT NULL DEFAULT 0, paced_ms INTEGER NOT NULL DEFAULT 0,
                delivery TEXT NOT NULL DEFAULT 'reject', expected_turn INTEGER,
                cached_input_tokens INTEGER NOT NULL DEFAULT 0,
                UNIQUE(bot,request_id));
            CREATE INDEX IF NOT EXISTS turns_bot_id ON turns(bot,id);
            CREATE INDEX IF NOT EXISTS turns_prompt_node ON turns(prompt_node) WHERE prompt_node IS NOT NULL;
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
                stream TEXT NOT NULL, data BLOB NOT NULL, raw_bytes INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY(turn,call_id,stream));
            CREATE TABLE IF NOT EXISTS events(id INTEGER PRIMARY KEY AUTOINCREMENT, bot TEXT NOT NULL REFERENCES bots(name),
                turn INTEGER, kind TEXT NOT NULL, data TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS event_retention(singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                pruned_cursor INTEGER NOT NULL);
            INSERT OR IGNORE INTO event_retention VALUES (1,0);
            CREATE INDEX IF NOT EXISTS events_bot_cursor ON events(bot,id);
            CREATE INDEX IF NOT EXISTS events_turn_kind_cursor ON events(turn,kind,id);
            CREATE INDEX IF NOT EXISTS checkpoints_head ON checkpoints(head);
            CREATE INDEX IF NOT EXISTS turns_running ON turns(id) WHERE status='running';
            CREATE INDEX IF NOT EXISTS turns_waiting ON turns(id) WHERE status='waiting';
            CREATE INDEX IF NOT EXISTS turns_paced ON turns(id) WHERE status='paced';
            CREATE INDEX IF NOT EXISTS turns_queued ON turns(bot,id) WHERE status='queued';
            CREATE INDEX IF NOT EXISTS turns_steers ON turns(bot,id) WHERE status='queued' AND delivery='steer';
            CREATE INDEX IF NOT EXISTS turns_ready ON turns(id) WHERE status='ready';
            CREATE INDEX IF NOT EXISTS turns_ready_bot ON turns(bot) WHERE status='ready';
            CREATE INDEX IF NOT EXISTS processes_running ON processes(turn) WHERE status='running';
            CREATE INDEX IF NOT EXISTS bots_deleting ON bots(name) WHERE status='deleting';")?;
        if version != Self::SCHEMA {
            tx.pragma_update(None, "user_version", Self::SCHEMA)?;
        }
        tx.commit()?;
        // Background command ownership is lost across daemon restart. This
        // does not prove the OS process stopped; never reuse its handle.
        conn.execute(
            "UPDATE processes SET status='lost',result=? WHERE status='running'",
            [json!({"error":"process_lost"}).to_string()],
        )?;
        let mut db = Self {
            conn,
            pending_limits: (0, 0),
            pending: (0, 0),
            outcomes: Vec::new(),
            pending_stale: false,
        };
        // A deletion interrupted between pieces finishes now: the bot was
        // already refusing work, and nothing else may see it half gone.
        let deleting: Vec<String> = db
            .conn
            .prepare("SELECT name FROM bots WHERE status='deleting'")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for name in deleting {
            db.delete_bot(&name)?;
        }
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
        // Queued turns outlive a restart. The oldest one of each idle bot
        // waits only for a slot; those behind a parked turn stay queued.
        db.conn.execute(
            "UPDATE turns SET status='ready' WHERE id IN (
                SELECT MIN(t.id) FROM turns t JOIN bots b ON b.name=t.bot
                WHERE t.status='queued' AND b.running_turn IS NULL
                  AND NOT EXISTS(SELECT 1 FROM turns r WHERE r.bot=t.bot AND r.status='ready')
                GROUP BY t.bot)",
            [],
        )?;
        db.recount_pending()?;
        Ok(db)
    }
    /// Count the waiting turns from their rows: one pass over the queued
    /// and ready rows, through their partial indexes.
    fn recount_pending(&mut self) -> Result<()> {
        self.pending_stale = true;
        self.pending = (0, 0);
        for statement in [
            "SELECT COUNT(*),COALESCE(SUM(length(CAST(prompt AS BLOB))),0) FROM turns WHERE status='queued'",
            "SELECT COUNT(*),COALESCE(SUM(length(CAST(prompt AS BLOB))),0) FROM turns WHERE status='ready'",
        ] {
            let (turns, bytes): (i64, i64) = self
                .conn
                .query_row(statement, [], |r| Ok((r.get(0)?, r.get(1)?)))?;
            self.pending.0 += turns;
            self.pending.1 += bytes;
        }
        self.pending_stale = false;
        Ok(())
    }
    /// Open the transaction a group of jobs shares. Each job's own
    /// transaction is a savepoint inside it, so a failed job rolls back
    /// alone and the group still commits once.
    pub fn begin_group(&mut self) -> Result<()> {
        // A group that could not be abandoned cleanly finishes that first:
        // admission must not run on counts the rolled-back jobs changed.
        if self.pending_stale {
            self.abandon_group()?;
        }
        self.conn.prepare_cached("BEGIN")?.execute([])?;
        Ok(())
    }
    /// Whether the group's transaction is still open. A full disk or an I/O
    /// error can make SQLite roll back the whole transaction mid-job.
    pub fn in_group(&self) -> bool {
        !self.conn.is_autocommit()
    }
    /// Commit the group: one sync for every job in it.
    pub fn commit_group(&mut self) -> Result<()> {
        if !self.in_group() {
            return fail("storage_group_rolled_back");
        }
        self.conn.prepare_cached("COMMIT")?.execute([])?;
        Ok(())
    }
    /// Forget a group that did not commit: roll back whatever is still
    /// open, drop the outcomes its jobs announced, and recount the waiting
    /// turns they counted.
    pub fn abandon_group(&mut self) -> Result<()> {
        self.outcomes.clear();
        self.pending_stale = true;
        if self.in_group() {
            self.conn.prepare_cached("ROLLBACK")?.execute([])?;
        }
        self.recount_pending()
    }
    /// The newest committed event id: the publication watermark at open.
    /// Older events belong to replay, never to live delivery.
    pub fn last_event_id(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COALESCE(MAX(id),0) FROM events", [], |r| r.get(0))?)
    }
    /// A turn's outcome for its waiters, published after this job's events.
    pub fn announce(&mut self, bot: &str, turn: i64, outcome: Value) {
        self.outcomes.push((bot.to_owned(), turn, outcome));
    }
    /// Hand everything the last job committed to the publisher, in commit
    /// order: events past the watermark, then announced outcomes. Only
    /// committed rows are read, so a job that failed mid-transaction
    /// publishes nothing. Stops early when the sink is gone.
    pub fn publish_since(
        &mut self,
        watermark: &mut i64,
        mut sink: impl FnMut(Publication) -> bool,
    ) -> Result<()> {
        loop {
            let mut statement = self.conn.prepare_cached(
                "SELECT id,bot,turn,kind,data FROM events WHERE id>? ORDER BY id LIMIT 256",
            )?;
            let mut rows = statement.query([*watermark])?;
            let mut delivered = 0;
            while let Some(row) = rows.next()? {
                let cursor: i64 = row.get(0)?;
                let data: Value = serde_json::from_str(&row.get::<_, String>(4)?)?;
                let event = entry(
                    cursor,
                    &row.get::<_, String>(1)?,
                    row.get(2)?,
                    &row.get::<_, String>(3)?,
                    data,
                );
                *watermark = cursor;
                delivered += 1;
                if !sink(Publication::Event(event)) {
                    self.outcomes.clear();
                    return Ok(());
                }
            }
            if delivered < 256 {
                break;
            }
        }
        for (bot, turn, outcome) in self.outcomes.drain(..) {
            if !sink(Publication::Finished { bot, turn, outcome }) {
                break;
            }
        }
        self.outcomes.clear();
        Ok(())
    }
    /// Whether a steer is queued for this bot: one indexed existence check,
    /// answered inside the jobs that start or resume its turn.
    pub fn steers_waiting(&self, name: &str) -> Result<bool> {
        Ok(self
            .conn
            .prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM turns WHERE bot=? AND status='queued' AND delivery='steer')",
            )?
            .query_row([name], |r| r.get(0))?)
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
            tools: split_tools(&r.get::<_, String>(12)?),
            input_tokens: r.get::<_, i64>(13)?.max(0) as u64,
            cached_input_tokens: r.get::<_, i64>(14)?.max(0) as u64,
            cache_hit: cache_hit(r.get::<_, i64>(14)?, r.get::<_, i64>(13)?),
            id: r.get(15)?,
            created_by: r.get(16)?,
            created_by_id: r.get(17)?,
            note: r.get(18)?,
            compaction: r.get(19)?,
            compaction_instructions: r.get(20)?,
            compaction_model: r.get(21)?,
            cache_bot: r.get(22)?,
            thinking_prefix: r.get(23)?,
            thinking_floor: r.get(24)?,
            fallbacks: r.get::<_, i64>(25)? != 0,
            elision: r.get(26)?,
        })
    }
    const COLUMNS: &str = "name,head,workspace,status,running_turn,provider,family,model,instructions,reasoning,budget_tokens,tokens_used,tools,input_tokens,cached_input_tokens,id,created_by,created_by_id,note,compaction,compaction_instructions,compaction_model,cache_bot,thinking_prefix,thinking_floor,fallbacks,elision";
    /// Validate the caller's captured identity in the same transaction that
    /// creates the child. Never resolve a stale shell's name to a new bot.
    fn creator_id(
        conn: &Connection,
        creator: Option<&str>,
        expected: Option<i64>,
    ) -> Result<Option<i64>> {
        match (creator, expected) {
            (None, None) => Ok(None),
            (Some(creator), Some(id)) => {
                let found = conn
                    .query_row(
                        "SELECT id FROM bots WHERE name=? AND id=? AND status!='deleting'",
                        params![creator, id],
                        |r| r.get::<_, i64>(0),
                    )
                    .optional()?;
                found.map(Some).ok_or(Error::new("creator_not_found"))
            }
            _ => fail("creator_identity_required"),
        }
    }
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
            "SELECT name,head,workspace,status,running_turn,provider,family,model,reasoning,budget_tokens,tokens_used,tools,
                    input_tokens,cached_input_tokens,id,created_by,created_by_id
             FROM bots WHERE name > ? ORDER BY name LIMIT ?",
        )?;
        let mut rows = statement.query(params![after.unwrap_or(""), (limit + 1) as i64])?;
        let mut bots = Vec::new();
        let mut bytes = 0;
        let mut more = false;
        while let Some(r) = rows.next()? {
            let bot = json!({"name":r.get::<_, String>(0)?,"id":r.get::<_, i64>(14)?,
                "head":r.get::<_, Option<i64>>(1)?,
                "workspace":r.get::<_, Option<String>>(2)?,"status":r.get::<_, String>(3)?,
                "running_turn":r.get::<_, Option<i64>>(4)?,"provider":r.get::<_, String>(5)?,
                "family":r.get::<_, String>(6)?,"model":r.get::<_, String>(7)?,
                "reasoning":r.get::<_, Option<String>>(8)?,
                "budget_tokens":r.get::<_, Option<i64>>(9)?,"tokens_used":r.get::<_, i64>(10)?,
                "tools":split_tools(&r.get::<_, String>(11)?),
                "input_tokens":r.get::<_, i64>(12)?,"cached_input_tokens":r.get::<_, i64>(13)?,
                "cache_hit":cache_hit(r.get::<_, i64>(13)?, r.get::<_, i64>(12)?),
                "created_by":r.get::<_, Option<String>>(15)?,
                "created_by_id":r.get::<_, Option<i64>>(16)?});
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
    ) -> Result<(Bot, Value)> {
        if self.exists(name)? {
            return fail("bot_exists");
        }
        let tx = self.conn.savepoint()?;
        let id = identity(&tx)?;
        let created_by_id = Self::creator_id(&tx, binding.created_by, binding.created_by_id)?;
        tx.execute(
            "INSERT INTO bots(name,id,head,workspace,status,running_turn,provider,family,model,instructions,reasoning,budget_tokens,tokens_used,context_start,pruned_cursor,tools,created_by,created_by_id,compaction_instructions,compaction_model,fallbacks) VALUES (?,?,NULL,?,'idle',NULL,?,?,?,?,?,?,0,NULL,0,?,?,?,?,?,?)",
            params![
                name,
                id,
                workspace,
                binding.provider,
                binding.family.name(),
                binding.model,
                binding.instructions,
                binding.reasoning,
                binding.budget_tokens.map(|b| b as i64),
                binding.tools.join(","),
                binding.created_by,
                created_by_id,
                binding.compaction_instructions,
                binding.compaction_model,
                binding.fallbacks
            ],
        )?;
        // The event carries the list record's fields, so a follower can
        // seat a new bot without a request per creation.
        let data = json!({"id":id,"provider":binding.provider,"model":binding.model,
            "workspace":workspace,"status":"idle","running_turn":null,
            "created_by":binding.created_by,"created_by_id":created_by_id});
        let cursor = event(&tx, name, None, "created", data.clone())?;
        tx.commit()?;
        Ok((
            self.inspect(name)?,
            entry(cursor, name, None, "created", data),
        ))
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
    /// boundary, with every tool result through the bot's elision floor
    /// counted, and sent, as its stub. The start is persisted and only moves
    /// when the budget is exceeded, then jumps back to about three quarters
    /// of the budget so the cached prefix stays stable across many turns.
    pub fn window(
        &mut self,
        name: &str,
        context_bytes: i64,
        context_items: i64,
    ) -> Result<Option<Window>> {
        // Heads only append and forks clear context_start, so the saved start
        // remains in this bot's ancestry. Read its accounting without walking
        // that ancestry or copying unrelated bot metadata such as instructions.
        // Byte totals are as sent: less what the stubs through the bot's
        // elision floor save (see `sent_total`).
        struct State {
            head: Option<i64>,
            family: String,
            total: i64,
            depth: i64,
            start: Option<i64>,
            start_depth: i64,
            before: i64,
            unsummarized: super::ContextUsage,
            history: bool,
            note: Option<(i64, String)>,
            compaction: Option<(i64, String, String, i64, i64)>,
            elided: i64,
            saved: i64,
        }
        let state = self
            .conn
            .prepare_cached(
                "SELECT b.head,b.family,COALESCE(h.total_bytes-MIN(h.total_elided,COALESCE(e.saved,0)),0),
                    COALESCE(h.depth,0),s.id,COALESCE(s.depth,0),
                    COALESCE(p.total_bytes-MIN(p.total_elided,COALESCE(e.saved,0)),0),
                    COALESCE(h.total_bytes-MIN(h.total_elided,COALESCE(e.saved,0)),0)
                        -COALESCE(cp.total_bytes-MIN(cp.total_elided,COALESCE(e.saved,0)),0),
                    COALESCE(h.depth,0)-COALESCE(cp.depth,0),
                    note.node,note.text,c.node,c.summary,c.prompts,c.covered_from,c.covered_to,
                    instr(','||b.tools||',',',history,')>0,COALESCE(e.through,0),COALESCE(e.saved,0)
             FROM bots b LEFT JOIN nodes h ON h.id=b.head
             LEFT JOIN nodes s ON s.id=b.context_start LEFT JOIN nodes p ON p.id=s.parent
             LEFT JOIN compactions c ON c.node=b.compaction LEFT JOIN nodes cut ON cut.id=c.cut
             LEFT JOIN nodes cp ON cp.id=cut.parent
             LEFT JOIN notes note ON note.node=b.note
             LEFT JOIN elisions e ON e.node=b.elision
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
                    unsummarized: super::ContextUsage {
                        bytes: r.get::<_, i64>(7)? as usize,
                        items: r.get::<_, i64>(8)? as usize,
                    },
                    history: r.get(16)?,
                    note: r
                        .get::<_, Option<i64>>(9)?
                        .map(|id| -> rusqlite::Result<_> { Ok((id, r.get(10)?)) })
                        .transpose()?,
                    compaction: r
                        .get::<_, Option<i64>>(11)?
                        .map(|id| -> rusqlite::Result<_> {
                            Ok((id, r.get(12)?, r.get(13)?, r.get(14)?, r.get(15)?))
                        })
                        .transpose()?,
                    elided: r.get(17)?,
                    saved: r.get(18)?,
                })
            })
            .optional()?
            .ok_or(Error::new("bot_not_found"))?;
        let Some(head) = state.head else {
            return Ok(None);
        };
        // One walk from the head: back to the saved start, or, without one,
        // over what could still fit. Sizes come from the cumulative columns;
        // no item is read here.
        struct Row {
            id: i64,
            parent: Option<i64>,
            depth: i64,
            /// Cumulative bytes as sent, through this node.
            total: i64,
            thinking: i64,
            turn_seq: Option<i64>,
        }
        let row = |r: &rusqlite::Row<'_>| {
            Ok(Row {
                id: r.get(0)?,
                parent: r.get(1)?,
                depth: r.get(2)?,
                total: r.get(3)?,
                thinking: r.get(4)?,
                turn_seq: r.get(5)?,
            })
        };
        let rows: Vec<Row> = match state.start {
            Some(_) => self
                .conn
                .prepare_cached(
                    "WITH RECURSIVE chain(id,parent,depth,total_bytes,thinking,turn_seq) AS (
                        SELECT id,parent,depth,total_bytes-MIN(total_elided,?3),thinking,turn_seq
                        FROM nodes WHERE id=?1
                        UNION ALL SELECT n.id,n.parent,n.depth,n.total_bytes-MIN(n.total_elided,?3),
                            n.thinking,n.turn_seq
                        FROM nodes n JOIN chain c ON n.id=c.parent WHERE c.depth>?2)
                     SELECT id,parent,depth,total_bytes,thinking,turn_seq FROM chain ORDER BY depth DESC",
                )?
                .query_map(params![head, state.start_depth, state.saved], row)?
                .collect::<rusqlite::Result<_>>()?,
            None => self
                .conn
                .prepare_cached(
                    "WITH RECURSIVE chain(id,parent,depth,total_bytes,thinking,turn_seq) AS (
                        SELECT id,parent,depth,total_bytes-MIN(total_elided,?6),thinking,turn_seq
                        FROM nodes WHERE id=?1
                        UNION ALL SELECT n.id,n.parent,n.depth,n.total_bytes-MIN(n.total_elided,?6),
                            n.thinking,n.turn_seq
                        FROM nodes n JOIN chain c ON n.id=c.parent
                        WHERE ?2 - c.total_bytes <= ?3 AND ?4 - c.depth <= ?5)
                     SELECT id,parent,depth,total_bytes,thinking,turn_seq FROM chain ORDER BY depth DESC",
                )?
                .query_map(
                    params![
                        head,
                        state.total,
                        context_bytes,
                        state.depth,
                        context_items,
                        state.saved
                    ],
                    row,
                )?
                .collect::<rusqlite::Result<_>>()?,
        };
        // Bytes before the oldest row: the saved start's parent, or else
        // the parent of wherever the walk stopped.
        let before = match (state.start, rows.last().and_then(|r| r.parent)) {
            (Some(_), _) => state.before,
            (None, Some(parent)) => self
                .conn
                .prepare_cached("SELECT total_bytes-MIN(total_elided,?2) FROM nodes WHERE id=?1")?
                .query_row(params![parent, state.saved], |r| r.get(0))?,
            (None, None) => 0,
        };
        // Newest first: what each row sends, and what a window starting at
        // each row sends in all.
        let mut sent = Vec::with_capacity(rows.len());
        let mut through = Vec::with_capacity(rows.len());
        for (index, row) in rows.iter().enumerate() {
            sent.push(row.total - rows.get(index + 1).map_or(before, |r| r.total));
            through.push(state.total - rows.get(index + 1).map_or(before, |r| r.total));
        }
        let fits = |index: usize, bytes_limit: i64, items_limit: i64| {
            through[index] + index as i64 <= bytes_limit && (index as i64) < items_limit
        };
        let saved_start = rows.len() - 1;
        let chosen = match state.start {
            Some(_) if fits(saved_start, context_bytes, context_items) => saved_start,
            _ => {
                // Over turn starts from the head, while the tail still fits,
                // keeping the oldest boundary under the target.
                let target_bytes = context_bytes / 4 * 3;
                let target_items = context_items / 4 * 3;
                let mut pick = None;
                for (index, row) in rows.iter().enumerate() {
                    if row.turn_seq.is_none() {
                        continue;
                    }
                    if !fits(index, context_bytes, context_items) {
                        break;
                    }
                    let within_target = fits(index, target_bytes, target_items);
                    if within_target || pick.is_none() {
                        pick = Some(index);
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
                    params![rows[pick].id, name],
                )?;
                pick
            }
        };
        let start = &rows[chosen];
        let item_bytes = through[chosen];
        let window = &rows[..=chosen];
        let compaction = match state.compaction {
            Some((version, summary, prompts, from, to)) => Some(CompactionView {
                version,
                summary,
                prompts: serde_json::from_str(&prompts)?,
                covered: (from, to),
            }),
            None => None,
        };
        let clamp = |bytes: i64| bytes.clamp(0, u32::MAX as i64) as u32;
        Ok(Some(Window {
            family: Family::parse(&state.family).ok_or(Error::new("store_family_unsupported"))?,
            ids: window.iter().rev().map(|r| r.id).collect(),
            sizes: sent[..=chosen].iter().rev().map(|&b| clamp(b)).collect(),
            thinking: window.iter().rev().map(|r| clamp(r.thinking)).collect(),
            item_bytes,
            unsummarized: state.unsummarized,
            omitted_items: start.depth - 1,
            omitted_turns: start.turn_seq.unwrap_or(1) - 1,
            history: state.history,
            note: state.note,
            compaction,
            elided: state.elided,
        }))
    }
    /// Where to move the bot's elision floor so the newest `keep_bytes` of
    /// its window stay verbatim, and what that saves. Only results the model
    /// has already answered go: the floor stays below its newest output.
    /// `None` when the move saves less than `min_saving`. Only the running
    /// turn moves the head, so the reader plans what the writer would.
    pub fn elision_plan(
        &self,
        name: &str,
        keep_bytes: i64,
        min_saving: i64,
    ) -> Result<Option<ElisionPlan>> {
        let state: Option<(i64, i64, i64, i64, i64)> = self
            .conn
            .prepare_cached(
                "SELECT b.head,s.depth,COALESCE(e.saved,0),
                    COALESCE(p.total_bytes-MIN(p.total_elided,COALESCE(e.saved,0)),0),COALESCE(e.through,0)
                 FROM bots b JOIN nodes s ON s.id=b.context_start LEFT JOIN nodes p ON p.id=s.parent
                 LEFT JOIN elisions e ON e.node=b.elision WHERE b.name=? AND b.head IS NOT NULL",
            )?
            .query_row([name], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .optional()?;
        let Some((head, start_depth, saved, before, elided)) = state else {
            return Ok(None);
        };
        // Newest first: id, cumulative bytes as sent, what its stub saves.
        let rows: Vec<(i64, i64, i64)> = self
            .conn
            .prepare_cached(
                "WITH RECURSIVE chain(id,parent,depth,total_bytes,elided) AS (
                    SELECT id,parent,depth,total_bytes-MIN(total_elided,?3),elided FROM nodes WHERE id=?1
                    UNION ALL SELECT n.id,n.parent,n.depth,n.total_bytes-MIN(n.total_elided,?3),n.elided
                    FROM nodes n JOIN chain c ON n.id=c.parent WHERE c.depth>?2)
                 SELECT id,total_bytes,elided FROM chain ORDER BY depth DESC",
            )?
            .query_map(params![head, start_depth, saved], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        // The newest model output: results after it are still unanswered.
        // A result with a stub is a result, so only the others are read.
        let mut item = self
            .conn
            .prepare_cached("SELECT item FROM nodes WHERE id=?")?;
        let mut answered = None;
        for (index, (id, _, saves)) in rows.iter().enumerate() {
            if *saves == 0
                && item.query_row([id], |r| {
                    Ok(super::context::model_output(r.get_ref(0)?.as_blob()?))
                })?
            {
                answered = Some(index);
                break;
            }
        }
        let Some(answered) = answered else {
            return Ok(None);
        };
        // Newest first, the verbatim tail takes rows while it stays within
        // `keep_bytes`; the floor goes through the first row it cannot take.
        let size = |index: usize| rows[index].1 - rows.get(index + 1).map_or(before, |r| r.1);
        let mut kept = 0;
        let mut edge = rows.len();
        for index in 0..rows.len() {
            kept += size(index);
            if kept > keep_bytes {
                edge = index;
                break;
            }
        }
        let floor = edge.max(answered);
        let Some(&(through, _, _)) = rows.get(floor) else {
            return Ok(None);
        };
        let (mut results, mut saved_bytes) = (0, 0);
        for row in &rows[floor..] {
            if let &(id, _, saves) = row
                && id > elided
                && saves > 0
            {
                results += 1;
                saved_bytes += saves;
            }
        }
        Ok(
            (results > 0 && saved_bytes >= min_saving).then_some(ElisionPlan {
                through,
                results,
                saved_bytes,
            }),
        )
    }
    /// Move the bot's elision floor as planned: a new version at the head,
    /// which forks from later checkpoints inherit.
    pub fn elide(&mut self, name: &str, plan: &ElisionPlan) -> Result<Value> {
        let bot = self.inspect(name)?;
        let head = bot.head.ok_or(Error::new("storage_error"))?;
        let elided: i64 = match bot.elision {
            Some(version) => self
                .conn
                .prepare_cached("SELECT through FROM elisions WHERE node=?")?
                .query_row([version], |r| r.get(0))?,
            None => 0,
        };
        if plan.through <= elided || plan.through > head || bot.elision == Some(head) {
            return fail("elision_not_forward");
        }
        let tx = self.conn.savepoint()?;
        // The version records what every stub through its floor saves, so
        // any node's bytes as sent are one subtraction (`sent_total`).
        tx.execute(
            "INSERT INTO elisions(node,previous,through,saved)
             SELECT ?,?,id,total_elided FROM nodes WHERE id=?",
            params![head, bot.elision, plan.through],
        )?;
        tx.execute(
            "UPDATE bots SET elision=? WHERE name=?",
            params![head, name],
        )?;
        let data = json!({"version":head,"previous":bot.elision,"through":plan.through,
            "results":plan.results,"saved_bytes":plan.saved_bytes});
        let cursor = event(&tx, name, bot.running_turn, "elided", data.clone())?;
        tx.commit()?;
        Ok(entry(cursor, name, bot.running_turn, "elided", data))
    }
    fn compaction_view(&self, name: &str) -> Result<Option<CompactionView>> {
        let row: Option<(i64, String, String, i64, i64)> = self
            .conn
            .prepare_cached(
                "SELECT c.node,c.summary,c.prompts,c.covered_from,c.covered_to
                 FROM bots b JOIN compactions c ON c.node=b.compaction WHERE b.name=?",
            )?
            .query_row([name], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .optional()?;
        Ok(match row {
            Some((version, summary, prompts, from, to)) => Some(CompactionView {
                version,
                summary,
                prompts: serde_json::from_str(&prompts)?,
                covered: (from, to),
            }),
            None => None,
        })
    }
    /// Bytes not yet covered by a summary: from the current compaction's cut,
    /// or the root, to the head. Equals the window's bytes until the window
    /// moves past the cut, which only an oversized backlog makes it do.
    /// One row read.
    pub fn unsummarized_bytes(&self, name: &str) -> Result<i64> {
        Ok(self
            .conn
            .prepare_cached(
                "SELECT COALESCE(h.total_bytes-MIN(h.total_elided,COALESCE(e.saved,0)),0)
                    -COALESCE(p.total_bytes-MIN(p.total_elided,COALESCE(e.saved,0)),0)
                 FROM bots b LEFT JOIN nodes h ON h.id=b.head
                 LEFT JOIN compactions c ON c.node=b.compaction
                 LEFT JOIN nodes cut ON cut.id=c.cut LEFT JOIN nodes p ON p.id=cut.parent
                 LEFT JOIN elisions e ON e.node=b.elision
                 WHERE b.name=?",
            )?
            .query_row([name], |r| r.get(0))?)
    }
    /// Choose what a compaction covers: walk back from the head over the
    /// span since the previous compaction, keep the newest whole turns that
    /// reach either the byte or item tail target, and summarize everything older,
    /// back to the previous cut. Returns nothing when no whole older turn
    /// exists to summarize. A span larger than the budget is caught up
    /// oldest first instead: this returns the walk to take in pieces with
    /// `catch_up_piece`, and `catch_up_plan` chooses from it.
    pub fn compaction_plan(
        &self,
        name: &str,
        keep_bytes: i64,
        keep_items: i64,
        max_bytes: i64,
        max_items: i64,
    ) -> Result<Option<Planning>> {
        let bot = self.inspect(name)?;
        let Some(head) = bot.head else {
            return Ok(None);
        };
        // A resumed model call must not summarize again at the same head.
        if bot.compaction == Some(head) {
            return Ok(None);
        }
        // Constant-count indexed reads size the span before any walk:
        // previous cut, head's totals, and the totals before the cut.
        // Bytes count as the summarizer is sent them: stubs through the
        // bot's elision floor, as the model last saw the span.
        type Span = (i64, i64, i64, i64, i64, i64, i64);
        let (previous_cut, head_total, head_depth, before, depth_before, elided, saved): Span =
            self.conn
                .prepare_cached(
                    "SELECT COALESCE(c.cut,-1),h.total_bytes-MIN(h.total_elided,COALESCE(e.saved,0)),
                        h.depth,COALESCE(p.total_bytes-MIN(p.total_elided,COALESCE(e.saved,0)),0),
                        COALESCE(p.depth,0),COALESCE(e.through,0),COALESCE(e.saved,0)
                 FROM nodes h LEFT JOIN compactions c ON c.node=?2
                 LEFT JOIN nodes cut ON cut.id=c.cut LEFT JOIN nodes p ON p.id=cut.parent
                 LEFT JOIN elisions e ON e.node=?3
                 WHERE h.id=?1",
                )?
                .query_row(params![head, bot.compaction, bot.elision], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                })?;
        if head_total - before > max_bytes || head_depth - depth_before > max_items {
            return Ok(Some(Planning::CatchUp(CatchUp {
                head,
                previous_cut,
                totals: (head_total, before, depth_before),
                elision: (elided, saved),
                bounds: (keep_bytes, keep_items, max_bytes, max_items),
                next: Some((head, None, None)),
                rows: Vec::new(),
            })));
        }
        // A source bot's turn records may be deleted while its nodes survive
        // in a fork. Decode only prompt nodes, outside the metadata-only walk,
        // and return bounded text rather than whole native items to Rust.
        let mut statement = self.conn.prepare_cached(
            "WITH RECURSIVE chain(id,parent,total_bytes,turn_seq) AS (
                SELECT id,parent,total_bytes-MIN(total_elided,?4),turn_seq FROM nodes WHERE id=?1
                UNION ALL SELECT n.id,n.parent,n.total_bytes-MIN(n.total_elided,?4),n.turn_seq
                FROM nodes n JOIN chain c ON n.id=c.parent
                WHERE c.id IS NOT ?2)
             SELECT c.id,c.total_bytes,COALESCE(p.total_bytes-MIN(p.total_elided,?4),0),c.turn_seq,
                CASE WHEN c.turn_seq IS NOT NULL THEN
                    (SELECT substr(json_extract(CAST(item AS TEXT),'$.content[0].text'),1,?3)
                     FROM nodes WHERE id=c.id) END
             FROM chain c LEFT JOIN nodes p ON p.id=c.parent ORDER BY c.id DESC",
        )?;
        // Newest first: id, own total, parent's total, ordinal, prompt.
        type SpanRow = (i64, i64, i64, Option<i64>, Option<String>);
        let rows: Vec<SpanRow> = statement
            .query_map(
                params![
                    head,
                    previous_cut,
                    Self::COMPACTION_PROMPT_BYTES as i64 + 1,
                    saved
                ],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )?
            .collect::<rusqlite::Result<_>>()?;
        // The newest prompt whose tail reaches either retention target.
        let mut cut = None;
        for (index, (id, _, before, seq, _)) in rows.iter().enumerate() {
            if seq.is_some()
                && (head_total - before >= keep_bytes || (index + 1) as i64 >= keep_items)
            {
                cut = Some((index, *id));
                break;
            }
        }
        let Some((cut_index, cut)) = cut else {
            return Ok(None);
        };
        // Everything older than the cut, back to and including the previous cut.
        let older = &rows[cut_index + 1..];
        if !older.iter().any(|r| r.3.is_some()) {
            return Ok(None);
        }
        let span = older
            .iter()
            .rev()
            .map(|(id, total, before, seq, prompt)| (*id, total - before, *seq, prompt.as_deref()));
        let previous_summary = self.previous_summary(&bot)?;
        let plan = Self::span_plan(
            cut,
            span,
            previous_summary,
            false,
            (max_bytes as usize / 3).min(64 * 1024),
            elided,
        );
        let (head_frame, tail_frame) = CompactionPlan::frame(
            bot.family()?,
            plan.previous_summary.as_deref(),
            plan.summary_bytes,
        )?;
        if plan.sizes.iter().map(|s| *s as usize).sum::<usize>()
            + plan.ids.len().saturating_sub(1)
            + head_frame.len()
            + tail_frame.len()
            > max_bytes as usize
            || plan.ids.len() + 1 + usize::from(plan.previous_summary.is_some())
                > max_items as usize
        {
            return Ok(Some(Planning::CatchUp(CatchUp {
                head,
                previous_cut,
                totals: (head_total, before, depth_before),
                elision: (elided, saved),
                bounds: (keep_bytes, keep_items, max_bytes, max_items),
                next: Some((head, None, None)),
                rows: Vec::new(),
            })));
        }
        Ok(Some(Planning::Plan(plan)))
    }
    /// One piece of a catch-up walk: at most `limit` nodes further back
    /// along the head's lineage toward the previous cut, reading node
    /// metadata only and decoding prompts only inside the budget. Separate
    /// pieces let other reads run between them.
    pub fn catch_up_piece(&self, walk: &mut CatchUp, limit: i64) -> Result<()> {
        let Some((from, child, child_seq)) = walk.next else {
            return Ok(());
        };
        let (_, before, depth_before) = walk.totals;
        let (_, _, max_bytes, max_items) = walk.bounds;
        let (_, saved) = walk.elision;
        // Each row carries its child on the lineage, so cutting at a prompt
        // child needs no parent lookup: the span ends at this row. Only rows
        // inside the budget and the piece's oldest row leave SQLite.
        let mut statement = self.conn.prepare_cached(
            "WITH RECURSIVE chain(id,parent,depth,total_bytes,turn_seq,child,child_seq,k) AS (
                SELECT id,parent,depth,total_bytes-MIN(total_elided,?9),turn_seq,?3,?4,1
                FROM nodes WHERE id=?1
                UNION ALL SELECT n.id,n.parent,n.depth,n.total_bytes-MIN(n.total_elided,?9),
                    n.turn_seq,c.id,c.turn_seq,c.k+1
                FROM nodes n JOIN chain c ON n.id=c.parent WHERE c.id IS NOT ?2 AND c.k<?5)
             SELECT c.id,c.parent,c.depth,c.total_bytes,c.turn_seq,c.child,c.child_seq,
                CASE WHEN c.turn_seq IS NOT NULL AND c.total_bytes<=?7 AND c.depth<=?8 THEN
                    (SELECT substr(json_extract(CAST(item AS TEXT),'$.content[0].text'),1,?6)
                     FROM nodes WHERE id=c.id) END
             FROM chain c WHERE (c.total_bytes<=?7 AND c.depth<=?8)
                OR c.k=?5 OR c.id IS ?2 OR c.parent IS NULL",
        )?;
        let (max_total, max_depth) = (
            before.saturating_add(max_bytes),
            depth_before.saturating_add(max_items),
        );
        let mut rows = statement.query(params![
            from,
            walk.previous_cut,
            child,
            child_seq,
            limit.max(1),
            Self::COMPACTION_PROMPT_BYTES as i64 + 1,
            max_total,
            max_depth,
            saved
        ])?;
        let mut last: Option<(i64, i64, Option<i64>, Option<i64>)> = None;
        while let Some(r) = rows.next()? {
            let (id, parent, depth, total, seq): (i64, Option<i64>, i64, i64, Option<i64>) =
                (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?);
            if total <= max_total && depth <= max_depth {
                walk.rows
                    .push((id, depth, total, seq, r.get(5)?, r.get(6)?, r.get(7)?));
            }
            if last.is_none_or(|(_, oldest, _, _)| depth < oldest) {
                last = Some((id, depth, parent, seq));
            }
        }
        walk.next = match last {
            Some((id, _, Some(parent), seq)) if id != walk.previous_cut => {
                Some((parent, Some(id), seq))
            }
            _ => None,
        };
        Ok(())
    }
    /// Choose a finished catch-up walk's step: the longest run of whole
    /// turns from the previous cut whose summarizer request, previous
    /// summary included, fits the budget, cut at the next prompt.
    pub fn catch_up_plan(&self, name: &str, walk: CatchUp) -> Result<Option<CompactionPlan>> {
        let bot = self.inspect(name)?;
        if walk.next.is_some() || bot.head != Some(walk.head) {
            return fail("compaction_walk_incomplete");
        }
        let CatchUp {
            totals: (head_total, before, depth_before),
            elision: (elided, _),
            bounds: (keep_bytes, keep_items, max_bytes, max_items),
            mut rows,
            ..
        } = walk;
        // Oldest first.
        rows.sort_unstable_by_key(|row| row.1);
        // The request carries the previous summary and the request to write
        // besides the span, and a comma between the span's items.
        let previous_summary = self.previous_summary(&bot)?;
        let (head_frame, tail_frame) = CompactionPlan::frame(
            bot.family()?,
            previous_summary.as_deref(),
            (max_bytes as usize / 3).min(64 * 1024),
        )?;
        let span_bytes = max_bytes - (head_frame.len() + tail_frame.len()) as i64 + 1;
        let span_items = max_items - 1 - previous_summary.is_some() as i64;
        // The span ending at row i is cut at its child, which must start a
        // turn, leave keep_bytes verbatim, and follow at least one prompt.
        let head_depth: i64 =
            self.conn
                .query_row("SELECT depth FROM nodes WHERE id=?", [walk.head], |r| {
                    r.get(0)
                })?;
        let mut prompted = false;
        let mut end = None;
        for (index, (_, depth, total, seq, _, child_seq, _)) in rows.iter().enumerate() {
            prompted |= seq.is_some();
            let items = depth - depth_before;
            if items > span_items || total - before + items > span_bytes {
                break;
            }
            if prompted
                && child_seq.is_some()
                && (head_total - total >= keep_bytes || head_depth - depth >= keep_items)
            {
                end = Some(index);
            }
        }
        let Some(end) = end else {
            return fail_with(
                "compaction_span_limit",
                "the oldest unsummarized turn exceeds the context budget; original history remains available",
            );
        };
        // The cut is the prompt that follows the span on the head's lineage.
        let cut = rows[end].4.ok_or(Error::new("storage_error"))?;
        let mut previous = before;
        let span = rows[..=end]
            .iter()
            .map(|(id, _, total, seq, _, _, prompt)| {
                let size = total - previous;
                previous = *total;
                (*id, size, *seq, prompt.as_deref())
            });
        Ok(Some(Self::span_plan(
            cut,
            span,
            previous_summary,
            true,
            (max_bytes as usize / 3).min(64 * 1024),
            elided,
        )))
    }
    fn previous_summary(&self, bot: &Bot) -> Result<Option<String>> {
        Ok(match bot.compaction {
            Some(node) => self
                .conn
                .prepare_cached("SELECT summary FROM compactions WHERE node=?")?
                .query_row([node], |r| r.get(0))
                .optional()?,
            None => None,
        })
    }
    /// A plan from the span's nodes, oldest first: id, own size, ordinal,
    /// and the prompt's bounded text for prompt nodes.
    fn span_plan<'a>(
        cut: i64,
        span: impl Iterator<Item = (i64, i64, Option<i64>, Option<&'a str>)>,
        previous_summary: Option<String>,
        catch_up: bool,
        summary_bytes: usize,
        elided: i64,
    ) -> CompactionPlan {
        let (mut ids, mut sizes, mut prompts) = (Vec::new(), Vec::new(), Vec::new());
        let (mut from, mut to) = (i64::MAX, 0);
        for (id, size, seq, prompt) in span {
            ids.push(id);
            sizes.push(size.clamp(0, u32::MAX as i64) as u32);
            if let (Some(seq), Some(prompt)) = (seq, prompt) {
                from = from.min(seq);
                to = to.max(seq);
                prompts.push((seq, bounded_prompt(prompt, Self::COMPACTION_PROMPT_BYTES)));
            }
        }
        bound_prompts(&mut prompts);
        CompactionPlan {
            cut,
            ids,
            sizes,
            prompts,
            covered: (from, to),
            previous_summary,
            catch_up,
            summary_bytes,
            elided,
        }
    }
    /// Record a compaction at the current head. The separate cut marks the
    /// context start; branches may independently summarize the same cut.
    /// One transaction, published like any event.
    pub fn compact(
        &mut self,
        name: &str,
        plan: &CompactionPlan,
        summary: &str,
        usage: Option<&Usage>,
        note_turns: usize,
        input_limit: super::ContextUsage,
    ) -> Result<Value> {
        let bot = self.inspect(name)?;
        // A compaction stands for everything since the first: the summary
        // merged the previous one, and the kept prompts carry over, bounded.
        let (mut prompts, mut covered_from) = (Vec::new(), plan.covered.0);
        if let Some(previous) = bot.compaction {
            let (kept, from): (String, i64) = self
                .conn
                .prepare_cached("SELECT prompts,covered_from FROM compactions WHERE node=?")?
                .query_row([previous], |r| Ok((r.get(0)?, r.get(1)?)))?;
            prompts = serde_json::from_str::<Vec<(i64, String)>>(&kept)?;
            covered_from = from;
        }
        prompts.extend(plan.prompts.iter().cloned());
        bound_prompts(&mut prompts);
        // Assess the complete logical view, not just the summary's text. A
        // catch-up step may still exceed the input budget; it must buy room
        // without inflating either resource. Reads use indexed totals plus
        // the same bounded omission listing and encoding as model requests.
        let mut candidate = CompactionView {
            version: bot.head.ok_or(Error::new("storage_error"))?,
            summary: summary.to_owned(),
            prompts,
            covered: (covered_from, plan.covered.1),
        };
        candidate.bound_prompt_bytes(bot.family()?, input_limit.bytes / 2)?;
        let old = self.compaction_view(name)?;
        let old_cut = bot
            .compaction
            .map(|id| {
                self.conn
                    .query_row("SELECT cut FROM compactions WHERE node=?", [id], |r| {
                        r.get::<_, i64>(0)
                    })
            })
            .transpose()?;
        let before =
            self.compaction_context_usage(&bot, old_cut, old.as_ref(), note_turns, input_limit)?;
        let after = self.compaction_context_usage(
            &bot,
            Some(plan.cut),
            Some(&candidate),
            note_turns,
            input_limit,
        )?;
        if after.bytes >= before.bytes || after.items > before.items {
            return fail_with(
                "compaction_not_smaller",
                "the summary, retained prompts and omission listing did not reduce the context view",
            );
        }
        // Catch-up may leave an oversized backlog, but it must not install a
        // pinned prefix that makes even the active turn impossible to send.
        // Optional previews yield during request fitting. Exclude them from
        // this minimum check, also avoiding another omitted-turn traversal.
        if !after.fits(input_limit)
            && let Some(turn) = bot.running_turn
        {
            let start: i64 =
                self.conn
                    .query_row("SELECT id FROM nodes WHERE turn=?", [turn], |r| r.get(0))?;
            let minimum =
                self.compaction_context_usage(&bot, Some(start), Some(&candidate), 0, input_limit)?;
            if !minimum.fits(input_limit) {
                return fail_with(
                    "compaction_context_limit",
                    "the new pinned prefix leaves insufficient room for the active turn",
                );
            }
        }
        let tx = self.conn.savepoint()?;
        tx.execute(
            "INSERT INTO compactions(node,previous,cut,summary,prompts,covered_from,covered_to) VALUES (?,?,?,?,?,?,?)",
            params![
                bot.head,
                bot.compaction,
                plan.cut,
                summary,
                serde_json::to_string(&candidate.prompts)?,
                covered_from,
                plan.covered.1
            ],
        )?;
        tx.execute(
            "UPDATE bots SET compaction=?,context_start=? WHERE name=?",
            params![bot.head, plan.cut, name],
        )?;
        let data = json!({"version":bot.head,"cut":plan.cut,"previous":bot.compaction,"covered_turns":[covered_from, plan.covered.1],
            "span_turns":[plan.covered.0, plan.covered.1],
            "items":plan.ids.len(),"bytes":plan.sizes.iter().map(|s| *s as u64).sum::<u64>(),
            "summary_bytes":summary.len(),"prompt_bytes":plan.prompts.iter().map(|(_, p)| p.len()).sum::<usize>(),
            "catch_up":plan.catch_up,
            "context_before":before,"context_after":after,
            "input_limit":input_limit,
            "headroom_bytes":input_limit.bytes as i64 - after.bytes as i64,
            "headroom_items":input_limit.items as i64 - after.items as i64,
            "reclaimed_bytes":before.bytes - after.bytes,
            "reclaimed_items":before.items - after.items});
        // Successful summaries and their accounting share one fsync/commit.
        if let Some(turn) = bot.running_turn {
            if let Some(usage) = usage {
                record_usage_for(&tx, name, turn, usage, Some("compaction"))?;
            }
            tx.execute(
                "UPDATE turns SET model_rounds=model_rounds+1 WHERE id=?",
                [turn],
            )?;
        }
        let cursor = event(&tx, name, bot.running_turn, "compacted", data.clone())?;
        tx.commit()?;
        Ok(entry(cursor, name, bot.running_turn, "compacted", data))
    }
    fn compaction_context_usage(
        &self,
        bot: &Bot,
        cut: Option<i64>,
        view: Option<&CompactionView>,
        note_turns: usize,
        input_limit: super::ContextUsage,
    ) -> Result<super::ContextUsage> {
        let (bytes, items, omitted, turns): (i64, i64, i64, i64) = self.conn.query_row(
            "SELECT h.total_bytes-MIN(h.total_elided,COALESCE(e.saved,0))
                    -COALESCE(p.total_bytes-MIN(p.total_elided,COALESCE(e.saved,0)),0),
                h.depth-COALESCE(p.depth,0),COALESCE(p.depth,0),COALESCE(c.turn_seq,1)-1
             FROM nodes h LEFT JOIN nodes c ON c.id=?2 LEFT JOIN nodes p ON p.id=c.parent
             LEFT JOIN elisions e ON e.node=?3 WHERE h.id=?1",
            params![bot.head, cut, bot.elision],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        let note: Option<(i64, String)> = self
            .conn
            .query_row(
                "SELECT node,text FROM notes WHERE node=?",
                [bot.note],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let listed = match cut {
            Some(cut) if note_turns > 0 && omitted > 0 => self.omitted_turns(cut, note_turns)?,
            _ => Vec::new(),
        };
        let prefix = super::context::context_prefix(
            bot.family()?,
            view,
            note.as_ref(),
            omitted,
            turns,
            bot.tools.iter().any(|tool| tool == "history"),
            &listed,
            input_limit.bytes * 2 / 3,
        )?;
        Ok(super::ContextUsage {
            bytes: bytes as usize,
            items: items as usize,
        }
        .with_prefix(&prefix))
    }
    /// The turns a window omits, newest first, at most `limit`: each turn's
    /// ordinal along the lineage and how its prompt began. Walks back from
    /// the window's start over those turns' nodes only; a turn's prompt node
    /// is the one that carries an ordinal, so the walk stops expanding at the
    /// prompt below the oldest listed turn.
    pub fn omitted_turns(&self, start: i64, limit: usize) -> Result<Vec<(i64, String)>> {
        let mut statement = self.conn.prepare_cached(
            "WITH RECURSIVE chain(id,parent,turn_seq) AS (
                SELECT n.id,n.parent,n.turn_seq FROM nodes s JOIN nodes n ON n.id=s.parent WHERE s.id=?1
                UNION ALL SELECT n.id,n.parent,n.turn_seq FROM nodes n JOIN chain c ON n.id=c.parent
                WHERE c.turn_seq IS NULL OR c.turn_seq>?2)
             SELECT c.turn_seq,
                (SELECT substr(json_extract(CAST(item AS TEXT),'$.content[0].text'),1,400)
                 FROM nodes WHERE id=c.id) FROM chain c
             WHERE c.turn_seq>?2 ORDER BY c.turn_seq DESC",
        )?;
        let seq: Option<i64> = self
            .conn
            .prepare_cached("SELECT turn_seq FROM nodes WHERE id=?")?
            .query_row([start], |r| r.get(0))
            .optional()?
            .flatten();
        let Some(seq) = seq else {
            return Ok(Vec::new());
        };
        let floor = (seq - 1 - limit as i64).max(0);
        let rows = statement.query_map(params![start, floor], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (ordinal, prompt) = row?;
            out.push((ordinal, first_line(&prompt, 120)));
        }
        Ok(out)
    }
    /// Encoded items for a batch of window ids, in order, comma-separated.
    /// Items joined by commas; those with ids below `floor` go without
    /// their thinking blocks, and one that held only thinking is left out.
    pub fn items_by_ids(&self, ids: &[i64], floor: i64, elided: i64) -> Result<Vec<u8>> {
        let mut statement = self
            .conn
            .prepare_cached("SELECT item,thinking FROM nodes WHERE id=?")?;
        // A stub replaces its result without the result's row being read.
        let mut stubs = self
            .conn
            .prepare_cached("SELECT item FROM stubs WHERE node=?")?;
        let mut out = Vec::new();
        for id in ids {
            if *id <= elided
                && let Some(stub) = stubs
                    .query_row([id], |r| Ok(r.get_ref(0)?.as_blob()?.to_vec()))
                    .optional()?
            {
                if !out.is_empty() {
                    out.push(b',');
                }
                out.extend_from_slice(&stub);
                continue;
            }
            statement.query_row([id], |r| {
                let item = r.get_ref(0)?.as_blob()?;
                let item = match (*id < floor && r.get::<_, i64>(1)? > 0)
                    .then(|| super::without_thinking(item))
                    .flatten()
                {
                    // Thinking was all it held: left out.
                    Some(kept) if kept.is_empty() => return Ok(()),
                    Some(kept) => std::borrow::Cow::Owned(kept),
                    None => std::borrow::Cow::Borrowed(item),
                };
                if !out.is_empty() {
                    out.push(b',');
                }
                out.extend_from_slice(&item);
                Ok(())
            })?;
        }
        Ok(out)
    }
    /// Durable lineage, drawn at creation and copied with the database. The
    /// daemon also binds it to the physical file for its cache namespace.
    pub fn store_identity(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT identity FROM store WHERE singleton=1", [], |r| {
                r.get(0)
            })?)
    }

    /// Bytes each node loses without its thinking blocks.
    pub fn thinking_of(&self, ids: &[i64]) -> Result<Vec<u32>> {
        let mut statement = self
            .conn
            .prepare_cached("SELECT thinking FROM nodes WHERE id=?")?;
        ids.iter()
            .map(|id| {
                Ok(statement
                    .query_row([id], |r| r.get::<_, i64>(0))?
                    .clamp(0, u32::MAX as i64) as u32)
            })
            .collect()
    }
    /// Record the context fingerprint a bot's requests now start with and
    /// the first node whose thinking is bound to it.
    pub fn set_thinking(&mut self, name: &str, prefix: i64, floor: i64) -> Result<()> {
        self.conn
            .prepare_cached("UPDATE bots SET thinking_prefix=?,thinking_floor=? WHERE name=?")?
            .execute(params![prefix, floor, name])?;
        Ok(())
    }
    /// Bytes and items in the active turn alone. Older turns can be removed
    /// from the context window; the current turn cannot. The indexed first
    /// node and head supply cumulative totals without walking the turn.
    pub fn turn_usage(&self, name: &str, turn: i64) -> Result<(Family, usize, usize)> {
        let row: Option<(String, i64, i64)> = self
            .conn
            .prepare_cached(
                "SELECT b.family,
                    h.total_bytes-MIN(h.total_elided,COALESCE(e.saved,0))
                        -COALESCE(p.total_bytes-MIN(p.total_elided,COALESCE(e.saved,0)),0),
                    h.depth-COALESCE(p.depth,0)
             FROM bots b JOIN nodes h ON h.id=b.head JOIN nodes s ON s.turn=?2
             LEFT JOIN nodes p ON p.id=s.parent LEFT JOIN elisions e ON e.node=b.elision
             WHERE b.name=?1 AND b.running_turn=?2",
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

    /// Minimum request view a history result must join. Read indexed active
    /// turn totals and mandatory prefix metadata. Optional previews yield to
    /// the result when fitting the next request; admission never walks them
    /// or moves the bot's cached window start on the writer.
    pub fn history_usage(&self, name: &str, turn: i64) -> Result<(Family, super::ContextUsage)> {
        self.minimum_context_usage(name, turn, None)
    }

    /// Check a proposed note against the next request before persisting it.
    /// The result node becomes its version; reserve the maximum encoded ID
    /// width without querying the global node allocator or taking a writer lock.
    pub fn validate_note(
        &self,
        name: &str,
        turn: i64,
        call_id: &str,
        outcome: &Outcome,
        limit: super::ContextUsage,
    ) -> Result<()> {
        let note = (
            i64::MAX,
            outcome.note.clone().ok_or(Error::new("missing_note"))?,
        );
        let (family, mut used) = self.minimum_context_usage(name, turn, Some(&note))?;
        used.bytes += family.tool_result_item(call_id, &outcome.output)?.len() + 1;
        used.items += 1;
        if !used.fits(limit) {
            return crate::fail_with(
                "note_context_limit",
                "the encoded note and current turn exceed the context budget; previous note unchanged",
            );
        }
        Ok(())
    }

    fn minimum_context_usage(
        &self,
        name: &str,
        turn: i64,
        note_override: Option<&(i64, String)>,
    ) -> Result<(Family, super::ContextUsage)> {
        let (family, bytes, items) = self.turn_usage(name, turn)?;
        let (omitted, turns, history, note) = self
            .conn
            .prepare_cached(
                "SELECT s.depth-1,s.turn_seq-1,instr(','||b.tools||',',',history,')>0,n.node,n.text
             FROM bots b JOIN nodes s ON s.turn=?2 LEFT JOIN notes n ON n.node=b.note
             WHERE b.name=?1 AND b.running_turn=?2",
            )?
            .query_row(params![name, turn], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, bool>(2)?,
                    r.get::<_, Option<i64>>(3)?
                        .map(|id| -> rusqlite::Result<_> { Ok((id, r.get::<_, String>(4)?)) })
                        .transpose()?,
                ))
            })?;
        let prefix = super::context::context_prefix(
            family,
            self.compaction_view(name)?.as_ref(),
            note_override.or(note.as_ref()),
            omitted,
            turns,
            history,
            &[],
            0,
        )?;
        Ok((
            family,
            super::ContextUsage { bytes, items }.with_prefix(&prefix),
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

    /// Reconcile duplicates first, then validate the effective provider using
    /// the bot already loaded for admission, before any durable mutation.
    /// Bounds for submissions waiting to start; zero is unbounded.
    pub fn set_pending_limits(&mut self, turns: usize, bytes: usize) {
        self.pending_limits = (turns, bytes);
    }
    /// Queued and ready turns and their prompt bytes.
    pub fn pending(&self) -> Result<(i64, i64)> {
        Ok(self.pending)
    }
    fn pending_left(&mut self, prompt_bytes: usize) {
        self.pending.0 -= 1;
        self.pending.1 -= prompt_bytes as i64;
    }
    /// The identity currently holding `name`. A submission that carries the
    /// identity it was first made against is refused once the name belongs to
    /// another bot, so a late retry never becomes fresh work on a namesake.
    pub fn identity(&self, name: &str, expected: Option<i64>) -> Result<i64> {
        let row: Option<(i64, String)> = self
            .conn
            .prepare_cached("SELECT id,status FROM bots WHERE name=?")?
            .query_row([name], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        let Some((id, status)) = row else {
            return fail("bot_not_found");
        };
        if status == "deleting" {
            return fail_with("bot_not_found", format!("{name} is being deleted"));
        }
        match expected {
            Some(expected) if expected != id => fail_with(
                "bot_not_found",
                format!("{name} is identity {id}; identity {expected} no longer exists"),
            ),
            _ => Ok(id),
        }
    }
    pub fn begin(
        &mut self,
        name: &str,
        request_id: &str,
        prompt: &str,
        capacity: bool,
        options: &TurnOptions,
        validate: impl Fn(&Bot, Option<&str>) -> Result<()>,
    ) -> Result<Started> {
        // A bot being deleted refuses work before its turn rows are gone,
        // so a retry cannot be answered from records about to vanish.
        let deleting: bool = self
            .conn
            .prepare_cached("SELECT EXISTS(SELECT 1 FROM bots WHERE name=? AND status='deleting')")?
            .query_row([name], |r| r.get(0))?;
        if deleting {
            return fail_with("bot_not_found", format!("{name} is being deleted"));
        }
        let prior: Option<(i64, String, String, TurnOptions, Option<i64>)> = self
            .conn
            .query_row(
                "SELECT id,prompt,status,workspace,model,delivery,expected_turn,prompt_node
                 FROM turns WHERE bot=? AND request_id=?",
                params![name, request_id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        TurnOptions {
                            workspace: r.get(3)?,
                            model: r.get(4)?,
                            delivery: Delivery::parse(&r.get::<_, String>(5)?).unwrap_or_default(),
                            expected_turn: r.get(6)?,
                        },
                        r.get(7)?,
                    ))
                },
            )
            .optional()?;
        if let Some((turn, mut saved, status, saved_options, prompt_node)) = prior {
            if let Some(node) = prompt_node {
                saved = self.conn.prepare_cached(
                    "SELECT json_extract(CAST(item AS TEXT),'$.content[0].text') FROM nodes WHERE id=?"
                )?.query_row([node], |r| r.get(0))?;
            }
            if saved != prompt || saved_options != *options {
                return fail("idempotency_conflict");
            }
            return Ok(Started {
                turn,
                fresh: false,
                status: turn_status_name(&status),
                entry: None,
            });
        }
        let reject = options.delivery == Delivery::Reject;
        // Admission applies only to new work, before any durable mutation.
        if !capacity && reject {
            return fail("active_agent_limit");
        }
        let bot = self.inspect(name)?;
        // A strict steer is for one running turn; anything else is stale
        // before a byte is written.
        if let Some(expected) = options.expected_turn
            && (options.delivery != Delivery::Steer || bot.running_turn != Some(expected))
        {
            return fail("stale_turn");
        }
        if let Err(error) = validate(&bot, options.model.as_deref()) {
            // An omitted steer model inherits the active turn for absorption.
            // Only look it up when the default fails validation: ordinary
            // submissions and steers with valid defaults pay no extra query.
            let inherited: Option<String> = if options.delivery == Delivery::Steer
                && options.model.is_none()
                && let Some(turn) = bot.running_turn
            {
                self.conn
                    .prepare_cached(
                        "SELECT model FROM turns WHERE id=?1 AND model IS NOT NULL
                     AND (?2 IS NULL OR ?2=COALESCE(workspace,?3))",
                    )?
                    .query_row(params![turn, options.workspace, bot.workspace], |r| {
                        r.get(0)
                    })
                    .optional()?
            } else {
                None
            };
            match inherited {
                Some(model) => validate(&bot, Some(&model))?,
                None => return Err(error),
            }
            // Keep the submitted model unset. If it misses absorption, start()
            // must validate its own default rather than pinning this override.
        }
        let busy = bot.running_turn.is_some() || self.has_ready_turn(name)?;
        if busy && reject {
            // Name the ways past a busy bot as flags to copy; callers, models
            // included, do not act on a description of them.
            let wait = match bot.running_turn {
                Some(turn) => format!(
                    "turn {turn} is running; resend with --delivery steer --turn {turn} \
                     to add this to it, or --delivery queue to run it afterwards"
                ),
                None => "earlier work is waiting; resend with --delivery queue to run \
                         this after it"
                    .to_owned(),
            };
            // A fork needs a settled point: during a turn, the head the turn
            // started from. A first turn has none, so no fork is offered.
            let checkpoint = match bot.running_turn {
                Some(turn) => self
                    .conn
                    .query_row("SELECT parent FROM nodes WHERE turn=?", [turn], |row| {
                        row.get::<_, Option<i64>>(0)
                    })
                    .optional()?
                    .flatten()
                    .map(|node| format!(" --checkpoint {node}")),
                None => Some(String::new()),
            };
            return fail_with(
                "bot_busy",
                match checkpoint {
                    Some(checkpoint) => format!(
                        "{wait}; to ask without interrupting, fork --source {name}{checkpoint} \
                         --bot NEW and send it to NEW"
                    ),
                    None => wait,
                },
            );
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
        // Only the head of a bot's line is ready; the rest wait behind it.
        let status = if busy {
            "queued"
        } else if capacity {
            "running"
        } else {
            "ready"
        };
        // Work that would wait counts against the pending bounds; a refusal
        // writes nothing, like the active-turn bound.
        if status != "running" {
            let (limit_turns, limit_bytes) = self.pending_limits;
            if limit_turns > 0 || limit_bytes > 0 {
                let (turns, bytes) = self.pending()?;
                if limit_turns > 0 && turns >= limit_turns as i64 {
                    return fail_with(
                        "pending_limit",
                        format!("{turns} submissions are waiting; the bound is {limit_turns}"),
                    );
                }
                if limit_bytes > 0 && bytes + prompt.len() as i64 > limit_bytes as i64 {
                    return fail_with(
                        "pending_limit",
                        format!(
                            "{bytes} prompt bytes are waiting; this one would pass the bound of {limit_bytes}"
                        ),
                    );
                }
            }
        }
        let tx = self.conn.savepoint()?;
        // A deleted bot must never make an old turn handle refer to new work.
        let turn: i64 = tx
            .prepare_cached(
                "UPDATE turn_sequence SET last_id=last_id+1 WHERE singleton=1 RETURNING last_id",
            )?
            .query_row([], |r| r.get(0))?;
        tx.prepare_cached(
            "INSERT INTO turns(id,bot,request_id,prompt,status,workspace,model,delivery,expected_turn) VALUES (?,?,?,?,?,?,?,?,?)",
        )?.execute(
            params![turn, name, request_id, prompt, status, options.workspace, options.model, options.delivery.name(), options.expected_turn],
        )?;
        tx.prepare_cached("INSERT INTO retained_turns(turn,bot) VALUES (?,?)")?
            .execute(params![turn, name])?;
        let model = options
            .model
            .clone()
            .unwrap_or_else(|| format!("{}/{}", bot.provider, bot.model));
        let (kind, data) = if status == "running" {
            let head = start_locked(&tx, &bot, turn, prompt)?;
            (
                "accepted",
                json!({"request_id":request_id,"node":head,"workspace":workspace,"model":model}),
            )
        } else {
            (
                "queued",
                json!({"request_id":request_id,"status":status,"delivery":options.delivery.name(),
                    "workspace":workspace,"model":model}),
            )
        };
        let cursor = event(&tx, name, Some(turn), kind, data.clone())?;
        tx.commit()?;
        if status != "running" {
            self.pending.0 += 1;
            self.pending.1 += prompt.len() as i64;
        }
        Ok(Started {
            turn,
            fresh: true,
            status,
            entry: Some(entry(cursor, name, Some(turn), kind, data)),
        })
    }
    /// Start a queued or ready turn on an idle bot: its user item joins the
    /// lineage and the bot becomes busy. Returns the durable `accepted` entry
    /// and whether a steer already waits for this bot.
    pub fn start(
        &mut self,
        turn: i64,
        validate: impl FnOnce(&Bot, Option<&str>) -> Result<()>,
    ) -> Result<(Value, bool)> {
        let (name, request_id, prompt, status, workspace, model, strict): (
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            bool,
        ) = self
            .conn
            .query_row(
                "SELECT bot,request_id,prompt,status,workspace,model,expected_turn IS NOT NULL FROM turns WHERE id=?",
                [turn],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )
            .optional()?
            .ok_or(Error::new("turn_not_found"))?;
        if !matches!(status.as_str(), "queued" | "ready") {
            return fail("stale_turn");
        }
        if strict {
            // The turn it was for is over; it must not become new work.
            return fail("stale_turn");
        }
        let bot = self.inspect(&name)?;
        if bot.running_turn.is_some() {
            // Back in line behind the bot's work, so the dispatcher never
            // picks the same ready row twice.
            self.conn
                .execute("UPDATE turns SET status='queued' WHERE id=?", [turn])?;
            return fail("bot_busy");
        }
        if bot.budget_tokens.is_some_and(|b| bot.tokens_used >= b) {
            return fail("budget_exhausted");
        }
        validate(&bot, model.as_deref())?;
        let workspace = workspace
            .or(bot.workspace.clone())
            .ok_or(Error::new("workspace_required"))?;
        let model = model.unwrap_or_else(|| format!("{}/{}", bot.provider, bot.model));
        let tx = self.conn.savepoint()?;
        let head = start_locked(&tx, &bot, turn, &prompt)?;
        let data = json!({"request_id":request_id,"node":head,"workspace":workspace,"model":model});
        let cursor = event(&tx, &name, Some(turn), "accepted", data.clone())?;
        tx.commit()?;
        self.pending_left(prompt.len());
        let steers = self.steers_waiting(&name)?;
        Ok((entry(cursor, &name, Some(turn), "accepted", data), steers))
    }
    /// An idle bot's line has exactly one ready head, including after recovery.
    /// Only inspect that head; running/parked bots are checked by the caller.
    fn has_ready_turn(&self, name: &str) -> Result<bool> {
        Ok(self
            .conn
            .prepare_cached("SELECT EXISTS(SELECT 1 FROM turns WHERE bot=? AND status='ready')")?
            .query_row([name], |r| r.get(0))?)
    }
    /// The oldest turn waiting only for an active slot.
    pub fn next_ready(&self) -> Result<Option<(String, i64)>> {
        Ok(self
            .conn
            .prepare_cached("SELECT bot,id FROM turns WHERE status='ready' ORDER BY id LIMIT 1")?
            .query_row([], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?)
    }
    /// A turn's own status, for a caller that names the bot.
    pub fn turn_status(&self, name: &str, turn: i64) -> Result<String> {
        self.conn
            .prepare_cached("SELECT status FROM turns WHERE id=? AND bot=?")?
            .query_row(params![turn, name], |r| r.get(0))
            .optional()?
            .ok_or(Error::new("turn_not_found"))
    }
    /// End a turn that never started. A ready turn's place goes to the bot's
    /// next queued one.
    pub fn end_queued(&mut self, turn: i64, error: &Error) -> Result<(Vec<Value>, Value)> {
        let (name, status, prompt_bytes): (String, String, i64) = self
            .conn
            .query_row(
                "SELECT bot,status,length(CAST(prompt AS BLOB)) FROM turns WHERE id=?",
                [turn],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
            .ok_or(Error::new("turn_not_found"))?;
        if !matches!(status.as_str(), "queued" | "ready") {
            return fail("stale_turn");
        }
        let ended = if error.code == "cancelled" {
            "interrupted"
        } else {
            "failed"
        };
        let tx = self.conn.savepoint()?;
        tx.execute(
            "UPDATE turns SET status=?,finished_ms=? WHERE id=?",
            params![ended, epoch_ms(), turn],
        )?;
        if status == "ready" {
            promote(&tx, &name)?;
        }
        let data = json!({"status":ended,"checkpoint":Value::Null,"error":error.code,"detail":error.detail});
        let cursor = event(&tx, &name, Some(turn), "turn_finished", data.clone())?;
        tx.commit()?;
        self.pending_left(prompt_bytes as usize);
        let outcome = self
            .turn_outcome(&name, turn)?
            .ok_or_else(|| Error::new("stale_turn"))?;
        self.announce(&name, turn, outcome.clone());
        Ok((
            vec![entry(cursor, &name, Some(turn), "turn_finished", data)],
            outcome,
        ))
    }
    /// Deliver a bounded prefix of queued steers into the running turn.
    /// Stop at an incompatible explicit override; it and later steers stay
    /// queued, so a later message cannot overtake the deferred steer.
    pub fn absorb(
        &mut self,
        turn: i64,
        through: Option<i64>,
        context_bytes: usize,
        context_items: usize,
    ) -> Result<Absorbed> {
        let bot = self.active(turn)?;
        let through = match through {
            Some(id) => Some(id),
            None => self.conn.prepare_cached(
                "SELECT MAX(id) FROM turns WHERE bot=? AND status='queued' AND delivery='steer'",
            )?.query_row([&bot.name], |r| r.get::<_, Option<i64>>(0))?,
        };
        let Some(through) = through else {
            return Ok(Absorbed::default());
        };
        // Absorbed steers join the running turn's own items, which the window
        // must carry whole. Budget them against the same three-quarter target
        // the window keeps, less what the turn already holds; what does not
        // fit stays queued and starts as its own turn when the line moves.
        let (family, used_bytes, used_items) = self.turn_usage(&bot.name, turn)?;
        let mut room_bytes = (context_bytes / 4 * 3).saturating_sub(used_bytes);
        let mut room_items = (context_items / 4 * 3).saturating_sub(used_items);
        let mut steers: Vec<(i64, Vec<u8>, usize)> = Vec::new();
        let mut more = false;
        let mut capped = false;
        {
            // The partial index skips ordinary queued work. Read at most one
            // candidate beyond the byte budget, without copying its prompt.
            let mut statement = self.conn.prepare_cached(
                "SELECT s.id,s.prompt,length(CAST(s.prompt AS BLOB)),
                    (s.workspace IS NULL OR s.workspace=COALESCE(t.workspace,b.workspace))
                    AND (s.model IS NULL OR s.model=COALESCE(t.model,b.provider||'/'||b.model))
                 FROM turns s JOIN turns t ON t.id=?2 JOIN bots b ON b.name=s.bot
                 WHERE s.bot=?1 AND s.status='queued' AND s.delivery='steer' AND s.id<=?4
                   AND (s.expected_turn IS NULL OR s.expected_turn=?2)
                 ORDER BY s.id LIMIT ?3",
            )?;
            let mut rows =
                statement.query(params![bot.name, turn, STEER_BATCH_ITEMS as i64, through])?;
            let mut bytes = 0;
            while let Some(row) = rows.next()? {
                let size = row.get::<_, i64>(2)? as usize;
                if !row.get::<_, bool>(3)? {
                    break;
                }
                if size > STEER_BATCH_BYTES - bytes {
                    // An oversized store-level prompt must not spin forever.
                    // Protocol prompts are already capped at this same limit.
                    more = !steers.is_empty();
                    break;
                }
                bytes += size;
                let item = family.user_item(&row.get::<_, String>(1)?)?;
                if item.len() > room_bytes || room_items == 0 {
                    capped = true;
                    break;
                }
                room_bytes -= item.len();
                room_items -= 1;
                steers.push((row.get(0)?, item, size));
            }
        }
        let mut absorbed = Absorbed::default();
        if steers.is_empty() {
            return Ok(absorbed);
        }
        if !capped && (more || steers.len() == STEER_BATCH_ITEMS) {
            absorbed.next_through = Some(through);
        }
        let tx = self.conn.savepoint()?;
        let mut head = bot.head;
        let mut steered = Vec::with_capacity(steers.len());
        for (steer, item, size) in steers {
            let id = node(&tx, head, &item)?;
            head = Some(id);
            if size >= PROMPT_SHARE_BYTES {
                tx.execute("UPDATE turns SET status='steered',finished_ms=?,prompt='',prompt_node=? WHERE id=?",
                    params![epoch_ms(), id, steer])?;
            } else {
                tx.execute(
                    "UPDATE turns SET status='steered',finished_ms=? WHERE id=?",
                    params![epoch_ms(), steer],
                )?;
            }
            let data = json!({"status":"steered","into":turn,"node":id,"checkpoint":Value::Null,
                "error":Value::Null,"detail":Value::Null});
            let cursor = event(&tx, &bot.name, Some(steer), "turn_finished", data.clone())?;
            absorbed
                .entries
                .push(entry(cursor, &bot.name, Some(steer), "turn_finished", data));
            let data = json!({"from":steer,"node":id});
            let cursor = event(&tx, &bot.name, Some(turn), "steered", data.clone())?;
            absorbed
                .entries
                .push(entry(cursor, &bot.name, Some(turn), "steered", data));
            steered.push((steer, size));
        }
        tx.execute(
            "UPDATE bots SET head=? WHERE name=?",
            params![head, bot.name],
        )?;
        tx.commit()?;
        for (steer, size) in steered {
            self.pending_left(size);
            let outcome = self
                .turn_outcome(&bot.name, steer)?
                .ok_or_else(|| Error::new("stale_turn"))?;
            self.announce(&bot.name, steer, outcome.clone());
            absorbed.outcomes.push((steer, outcome));
        }
        Ok(absorbed)
    }
    pub fn append(
        &mut self,
        turn: i64,
        items: Vec<Bytes>,
        calls: &[ToolCall],
        usage: Option<&Usage>,
    ) -> Result<Vec<Value>> {
        let bot = self.active(turn)?;
        let tx = self.conn.savepoint()?;
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
        let tx = self.conn.savepoint()?;
        let entry = record_usage(&tx, &bot.name, turn, usage)?;
        tx.execute(
            "UPDATE turns SET model_rounds=model_rounds+1 WHERE id=?",
            [turn],
        )?;
        tx.commit()?;
        Ok(entry)
    }
    /// Charge a prompt-cache refresh sent while a tool ran. It generated
    /// nothing, so it is not a model round.
    pub fn keep_warm_usage(&mut self, turn: i64, usage: &Usage) -> Result<()> {
        let bot = self.active(turn)?;
        let tx = self.conn.savepoint()?;
        record_usage_for(&tx, &bot.name, turn, usage, Some("keep_warm"))?;
        tx.commit()?;
        Ok(())
    }
    /// Charge a summarizer response without adding it to the transcript.
    pub fn compaction_usage(&mut self, turn: i64, usage: Option<&Usage>) -> Result<()> {
        let bot = self.active(turn)?;
        let tx = self.conn.savepoint()?;
        if let Some(usage) = usage {
            record_usage_for(&tx, &bot.name, turn, usage, Some("compaction"))?;
        }
        tx.execute(
            "UPDATE turns SET model_rounds=model_rounds+1 WHERE id=?",
            [turn],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn tool_start(&mut self, turn: i64, call: &ToolCall) -> Result<Value> {
        let bot = self.active(turn)?;
        let tx = self.conn.savepoint()?;
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
        let family = bot.family()?;
        let item = family.tool_result_item(call_id, &outcome.output)?;
        let tx = self.conn.savepoint()?;
        if tx.execute(
            "UPDATE tools SET status='completed' WHERE turn=? AND call_id=? AND status='executing'",
            params![turn, call_id],
        )? != 1
        {
            return fail("invalid_tool_state");
        }
        for (stream, data) in &outcome.artifacts {
            artifact::put(&tx, turn, call_id, stream, data)?;
        }
        // The stub names the result's node, so it is made for the id the
        // insert takes, and its savings go in with the node. Written with
        // the result, eliding it later reads no output.
        let next = next_node(&tx)?;
        let stub = super::context::stub(family, call_id, &outcome.output, next, item.len())?;
        let elided = stub
            .as_ref()
            .map_or(0, |stub| (item.len() - stub.len()) as i64);
        let head = insert_node(&tx, bot.head, &item, None, elided)?;
        if head != next {
            return fail("storage_error");
        }
        if let Some(stub) = stub {
            tx.prepare_cached("INSERT INTO stubs(node,item) VALUES (?,?)")?
                .execute(params![head, stub])?;
        }
        tx.execute(
            "UPDATE bots SET head=? WHERE name=?",
            params![head, bot.name],
        )?;
        if let Some(text) = &outcome.note {
            // The result's node is the note's version: on this lineage by
            // construction, so a fork binds to it by position.
            tx.execute(
                "INSERT INTO notes(node,previous,text) VALUES (?,?,?)",
                params![head, bot.note, text],
            )?;
            tx.execute(
                "UPDATE bots SET note=? WHERE name=?",
                params![head, bot.name],
            )?;
        }
        let artifacts: Vec<&str> = outcome.artifacts.iter().map(|(s, _)| *s).collect();
        let data = json!({"call_id":call_id,"node":head,"artifacts":artifacts,
            "note":outcome.note.as_ref().map(|_| head)});
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
            created_by: bot.created_by,
            created_by_id: bot.created_by_id,
            bot: bot.name,
            bot_id: bot.id,
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
        let tx = self.conn.savepoint()?;
        let mut head = bot.head;
        let mut entries = Vec::new();
        // Complete the transcript, not the external operation. Dropped native
        // I/O and background commands can outlive cancellation or a crash.
        // Report missing outcomes honestly and leave the bot usable; never
        // automatically repeat a tool whose result was not committed.
        let pending: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM tools WHERE turn=? AND status IN ('planned','executing'))",
            [turn],
            |r| r.get(0),
        )?;
        if pending {
            let unanswered: Vec<(String, String)> = tx
                .prepare("SELECT call_id,status FROM tools WHERE turn=? AND status IN ('planned','executing') ORDER BY rowid")?
                .query_map([turn], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let family = bot.family()?;
            for (call_id, status) in unanswered {
                let unknown = status == "executing" && waiting.is_none();
                let detail = if unknown {
                    "turn ended before this tool's result was recorded; it may have had effects and may still be running. Inspect the current state before retrying"
                } else if waiting.is_some() && status == "executing" {
                    "turn interrupted while parked"
                } else {
                    "turn ended before this call ran"
                };
                let output = json!({"error":if unknown { "tool_outcome_unknown" } else { "cancelled" },"detail":detail}).to_string();
                let item = family.tool_result_item(&call_id, &output)?;
                let id = node(&tx, head, &item)?;
                head = Some(id);
                tx.execute(
                    "UPDATE tools SET status='completed' WHERE turn=? AND call_id=?",
                    params![turn, call_id],
                )?;
                let data = json!({"call_id":call_id,"node":id,"artifacts":[],"cancelled":!unknown,"outcome_unknown":unknown});
                let cursor = event(&tx, &bot.name, Some(turn), "tool_completed", data.clone())?;
                entries.push(entry(cursor, &bot.name, Some(turn), "tool_completed", data));
            }
        }
        if let Some(waiting) = &waiting {
            tx.execute(
                "UPDATE turns SET waiting=NULL,paced_ms=paced_ms+? WHERE id=?",
                params![waiting.paced_elapsed_ms(), turn],
            )?;
        }
        let code = error.map(|e| e.code.as_str());
        // Interrupted, by cause: a client's interrupt, the daemon shutting
        // down around the turn, or a daemon that died with it running.
        let status = if matches!(
            code,
            Some("cancelled" | "daemon_shutdown" | "process_interrupted")
        ) || (pending && code.is_none())
        {
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
        promote(&tx, &bot.name)?;
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
    #[allow(clippy::too_many_arguments)]
    pub fn suspend(
        &mut self,
        turn: i64,
        call_id: &str,
        handles: &[String],
        deadline_ms: Option<u64>,
        any: bool,
        pending: &[ToolCall],
        route: Option<&str>,
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
            any,
            pending: pending.to_vec(),
            paced_since_ms: None,
            call_attempts: 0,
            call_spent_ms: 0,
            compaction: false,
            route: route.map(str::to_owned),
        };
        let tx = self.conn.savepoint()?;
        tx.execute(
            "UPDATE turns SET status='waiting',waiting=? WHERE id=?",
            params![serde_json::to_string(&waiting)?, turn],
        )?;
        tx.execute("UPDATE bots SET status='waiting' WHERE name=?", [&bot.name])?;
        let data = json!({"call_id":call_id,"handles":handles,"deadline_ms":deadline_ms,"any":any});
        let cursor = event(&tx, &bot.name, Some(turn), "turn_waiting", data.clone())?;
        tx.commit()?;
        Ok(entry(cursor, &bot.name, Some(turn), "turn_waiting", data))
    }
    /// Park a running turn at its model-call boundary until `resume_at_ms`,
    /// because its provider's pool is closed by a rate limit. It holds no
    /// task and no slot until then; retries stay in the turn row.
    #[allow(clippy::too_many_arguments)]
    pub fn suspend_paced(
        &mut self,
        turn: i64,
        resume_at_ms: u64,
        call_attempts: u32,
        call_spent_ms: u64,
        retries: u64,
        paced_ms: u64,
        compaction: bool,
        route: Option<&str>,
    ) -> Result<Value> {
        let bot = self.active(turn)?;
        if bot.status != "running" {
            return fail("invalid_tool_state");
        }
        let waiting = Waiting {
            turn,
            bot: bot.name.clone(),
            call_id: String::new(),
            handles: Vec::new(),
            deadline_ms: Some(resume_at_ms),
            any: false,
            pending: Vec::new(),
            paced_since_ms: Some(epoch_ms()),
            call_attempts,
            call_spent_ms,
            compaction,
            route: route.map(str::to_owned),
        };
        let tx = self.conn.savepoint()?;
        tx.execute(
            "UPDATE turns SET status='paced',waiting=?,retries=retries+?,paced_ms=paced_ms+? WHERE id=?",
            params![serde_json::to_string(&waiting)?, retries as i64, paced_ms as i64, turn],
        )?;
        tx.execute("UPDATE bots SET status='paced' WHERE name=?", [&bot.name])?;
        let data = json!({"resume_at_ms":resume_at_ms});
        let cursor = event(&tx, &bot.name, Some(turn), "turn_paced", data.clone())?;
        tx.commit()?;
        Ok(entry(cursor, &bot.name, Some(turn), "turn_paced", data))
    }
    /// Every parked turn, on handles or on a pool, for re-registration after
    /// a restart.
    pub fn waiting_turns(&self) -> Result<Vec<Waiting>> {
        let mut statement = self.conn.prepare(
            "SELECT id FROM turns WHERE status='waiting'
             UNION ALL SELECT id FROM turns WHERE status='paced' ORDER BY id",
        )?;
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
                 WHERE name=? AND status IN ('waiting','paced') AND running_turn=?)",
            )?
            .query_row(params![name, turn], |r| r.get(0))?)
    }

    /// Bring a parked turn back to running; the caller then records the wait
    /// result. Also answers whether a steer queued while it was parked.
    pub fn resume(&mut self, turn: i64) -> Result<(Waiting, Value, bool)> {
        let waiting = self.waiting(turn)?.ok_or(Error::new("turn_not_waiting"))?;
        let bot = self.active(turn)?;
        if bot.status != "waiting" && bot.status != "paced" {
            return fail("turn_not_waiting");
        }
        let tx = self.conn.savepoint()?;
        tx.execute(
            "UPDATE turns SET status='running',waiting=NULL,paced_ms=paced_ms+? WHERE id=?",
            params![waiting.paced_elapsed_ms(), turn],
        )?;
        tx.execute("UPDATE bots SET status='running' WHERE name=?", [&bot.name])?;
        let data = json!({"call_id":waiting.call_id});
        let cursor = event(&tx, &bot.name, Some(turn), "turn_resumed", data.clone())?;
        tx.commit()?;
        let steers = self.steers_waiting(&bot.name)?;
        Ok((
            waiting,
            entry(cursor, &bot.name, Some(turn), "turn_resumed", data),
            steers,
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
        if matches!(
            status.as_str(),
            "running" | "waiting" | "paced" | "queued" | "ready"
        ) {
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
        let tx = self.conn.savepoint()?;
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
                artifact::put(&tx, turn, &call_id, stream, data)?;
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
    /// unanswered; the source itself is never changed. The fork copies the
    /// source's binding, instructions, and tools as they are, so its first
    /// call repeats the source's prefix and can read the source's cache.
    pub fn fork(&mut self, source: &str, name: &str, fork: Fork<'_>) -> Result<(Bot, Value)> {
        let Fork {
            checkpoint: node,
            workspace,
            budget_tokens,
            created_by,
            created_by_id,
        } = fork;
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
        if parent.status == "deleting" {
            return fail_with("bot_not_found", format!("{source} is being deleted"));
        }
        let tx = self.conn.savepoint()?;
        let id = identity(&tx)?;
        let created_by_id = Self::creator_id(&tx, created_by, created_by_id)?;
        tx.execute(
            "INSERT INTO bots(name,id,head,workspace,status,running_turn,provider,family,model,instructions,reasoning,budget_tokens,tokens_used,context_start,pruned_cursor,tools,created_by,created_by_id,compaction_instructions,compaction_model,cache_bot,thinking_prefix,thinking_floor,fallbacks) VALUES (?,?,?,?,'idle',NULL,?,?,?,?,?,?,0,NULL,0,?,?,?,?,?,?,?,?,?)",
            params![
                name,
                id,
                checkpoint,
                workspace,
                parent.provider,
                parent.family,
                parent.model,
                parent.instructions,
                parent.reasoning,
                budget_tokens.map(|b| b as i64),
                parent.tools.join(","),
                created_by,
                created_by_id,
                parent.compaction_instructions,
                parent.compaction_model,
                // The fork's first call can read the source's cache.
                parent.cache_bot(),
                // The source's thinking is bound to the context in front of
                // its window. Carry that over only if it was already in
                // place at the checkpoint; the fork's first request compares
                // its own context against it.
                (parent.thinking_floor <= checkpoint.map_or(0, |c| c + 1))
                    .then_some(parent.thinking_prefix)
                    .flatten(),
                parent.thinking_floor,
                parent.fallbacks
            ],
        )?;
        if let Some(node) = checkpoint {
            tx.execute("INSERT INTO checkpoints VALUES (?,?)", params![name, node])?;
            // The newest note version at or before the checkpoint, along
            // the source's version chain; ids grow along a lineage.
            let mut version = parent.note;
            while let Some(node_id) = version.filter(|v| *v > node) {
                version =
                    tx.query_row("SELECT previous FROM notes WHERE node=?", [node_id], |r| {
                        r.get(0)
                    })?;
            }
            tx.execute(
                "UPDATE bots SET note=? WHERE name=?",
                params![version, name],
            )?;
            let mut version = parent.compaction;
            while let Some(node_id) = version.filter(|v| *v > node) {
                version = tx.query_row(
                    "SELECT previous FROM compactions WHERE node=?",
                    [node_id],
                    |r| r.get(0),
                )?;
            }
            tx.execute(
                "UPDATE bots SET compaction=?1,context_start=(SELECT cut FROM compactions WHERE node=?1) WHERE name=?2",
                params![version, name],
            )?;
            // The elision floor the source had at the checkpoint.
            let mut version = parent.elision;
            while let Some(node_id) = version.filter(|v| *v > node) {
                version = tx.query_row(
                    "SELECT previous FROM elisions WHERE node=?",
                    [node_id],
                    |r| r.get(0),
                )?;
            }
            tx.execute(
                "UPDATE bots SET elision=? WHERE name=?",
                params![version, name],
            )?;
        }
        let data = json!({"id":id,"source":source,"checkpoint":checkpoint,"node":checkpoint,
            "provider":parent.provider,"model":parent.model,
            "workspace":workspace,"status":"idle","running_turn":null,
            "created_by":created_by,"created_by_id":created_by_id});
        let cursor = event(&tx, name, None, "forked", data.clone())?;
        tx.commit()?;
        Ok((
            self.inspect(name)?,
            entry(cursor, name, None, "forked", data),
        ))
    }
    pub fn events(&self, name: &str, after: i64, limit: usize) -> Result<Value> {
        self.inspect(name)?;
        self.event_page(Some(name), after, limit)
    }
    /// Every bot's durable events after a store-wide cursor: what a fleet
    /// controller replays on one connection instead of one per bot.
    pub fn events_after(&self, after: i64, limit: usize) -> Result<Value> {
        self.event_page(None, after, limit)
    }
    fn event_page(&self, name: Option<&str>, after: i64, limit: usize) -> Result<Value> {
        if after < 0 || !(1..=256).contains(&limit) {
            return fail("invalid_event_page");
        }
        let mut stmt = self.conn.prepare_cached(match name {
            Some(_) => "SELECT id,turn,kind,data,bot FROM events WHERE bot=?1 AND id>?2 ORDER BY id LIMIT ?3",
            None => "SELECT id,turn,kind,data,bot FROM events WHERE ?1 IS NULL AND id>?2 ORDER BY id LIMIT ?3",
        })?;
        let mut rows = stmt.query(params![name, after, limit as i64])?;
        let mut events = Vec::new();
        let mut cursor = after;
        // Leave ample room for the response envelope and its caller-supplied ID.
        let mut bytes = 0;
        let byte_limit = crate::output::MAX_EVENT / 2;
        while let Some(row) = rows.next()? {
            let next: i64 = row.get(0)?;
            let data: String = row.get(3)?;
            let bot: String = row.get(4)?;
            let item = entry(
                next,
                &bot,
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
        let pruned: i64 = match name {
            Some(name) => {
                self.conn
                    .query_row("SELECT pruned_cursor FROM bots WHERE name=?", [name], |r| {
                        r.get(0)
                    })?
            }
            // Any bot's retention gap is a gap in the store-wide stream.
            None => self.conn.query_row(
                "SELECT pruned_cursor FROM event_retention WHERE singleton=1",
                [],
                |r| r.get(0),
            )?,
        };
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
    /// Remove a bot and everything only it owns, running every piece to
    /// completion on this thread. Services run the pieces as separate jobs.
    pub fn delete_bot(&mut self, name: &str) -> Result<Value> {
        let (id, mut piece) = self.start_delete_bot(name, Self::RETENTION_PIECE)?;
        let mut totals = json!({"turns":0,"events":0,"nodes":0});
        loop {
            for key in ["turns", "events", "nodes"] {
                totals[key] =
                    json!(totals[key].as_i64().unwrap_or(0) + piece[key].as_i64().unwrap_or(0));
            }
            if piece["done"] == true {
                return Ok(totals);
            }
            piece = self.delete_bot_piece(name, id, Self::RETENTION_PIECE)?;
        }
    }
    /// Bind a synchronous deletion to the current identity and commit its
    /// first piece using the same loaded record.
    pub fn start_delete_bot(&mut self, name: &str, piece: usize) -> Result<(i64, Value)> {
        let bot = self.inspect(name)?;
        let id = bot.id;
        Ok((id, self.delete_bot_piece_for(name, bot, piece)?))
    }
    /// One bounded piece of a deletion. The first piece checks the bot is
    /// idle and marks it `deleting`, after which it refuses new work; each
    /// later piece drops the records of up to `piece` turns, then the turn
    /// rows, then up to `piece` nodes of the exclusive suffix with `head`
    /// moved back as they go, so an interruption resumes exactly. The last
    /// piece removes the bot row and answers `done`.
    pub fn delete_bot_piece(
        &mut self,
        name: &str,
        expected_id: i64,
        piece: usize,
    ) -> Result<Value> {
        let bot = self.inspect(name)?;
        if bot.id != expected_id {
            return fail("bot_not_found");
        }
        self.delete_bot_piece_for(name, bot, piece)
    }
    fn delete_bot_piece_for(&mut self, name: &str, bot: Bot, piece: usize) -> Result<Value> {
        let piece = piece.max(1) as i64;
        let tx = self.conn.savepoint()?;
        let mut out = json!({"turns":0,"events":0,"nodes":0,"done":false});
        if bot.status != "deleting" {
            if bot.running_turn.is_some() {
                return fail("bot_busy");
            }
            let running: bool = tx
                .prepare_cached(
                    "SELECT EXISTS(SELECT 1 FROM turns t JOIN processes p ON p.turn=t.id
                 WHERE t.bot=? AND p.status='running')
                 OR EXISTS(SELECT 1 FROM turns WHERE bot=? AND status IN ('queued','ready'))",
                )?
                .query_row([name, name], |r| r.get(0))?;
            if running {
                return fail("bot_busy");
            }
            // Preserve identity before freeing the suffix, in the same transaction.
            // The head is the largest ID on this append-only lineage. Together
            // with surviving nodes, this floor covers every committed node ID.
            if let Some(head) = bot.head {
                tx.execute(
                    "UPDATE node_sequence SET last_id=MAX(last_id,?) WHERE singleton=1",
                    [head],
                )?;
            }
            // Reserve the deletion's event range for both replay scopes before
            // any piece commits. RETURNING shares the existing indexed lookup
            // with the global watermark, without another query per piece.
            let pruned: i64 = tx.query_row(
                "UPDATE bots SET status='deleting',context_start=NULL,note=NULL,compaction=NULL,elision=NULL,
                    pruned_cursor=MAX(pruned_cursor,
                        COALESCE((SELECT MAX(id) FROM events WHERE bot=?1),0))
                 WHERE name=?1 RETURNING pruned_cursor",
                [name],
                |r| r.get(0),
            )?;
            tx.execute(
                "UPDATE event_retention SET pruned_cursor=MAX(pruned_cursor,?) WHERE singleton=1",
                [pruned],
            )?;
            // Its checkpoints reference nodes the walk below will free.
            tx.execute("DELETE FROM checkpoints WHERE bot=?", [name])?;
        }
        // Operational records, a piece of turns at a time.
        let turns: Vec<i64> = tx
            .prepare_cached("SELECT turn FROM retained_turns WHERE bot=? ORDER BY turn LIMIT ?")?
            .query_map(params![name, piece], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        if !turns.is_empty() {
            let mut events = 0;
            for turn in &turns {
                for table in ["artifacts", "processes", "tools"] {
                    tx.prepare_cached(&format!("DELETE FROM {table} WHERE turn=?"))?
                        .execute([turn])?;
                }
                events += tx
                    .prepare_cached("DELETE FROM events WHERE turn=?")?
                    .execute([turn])?;
                tx.prepare_cached("DELETE FROM retained_turns WHERE turn=?")?
                    .execute([turn])?;
            }
            out["events"] = json!(events);
            tx.commit()?;
            return Ok(out);
        }
        // Then the turn rows and what remains keyed by the bot alone.
        let has_turns: bool = tx
            .prepare_cached("SELECT EXISTS(SELECT 1 FROM turns WHERE bot=?)")?
            .query_row([name], |r| r.get(0))?;
        if has_turns {
            out["events"] = json!(tx.execute("DELETE FROM events WHERE bot=?", [name])?);
            out["turns"] = json!(tx.execute("DELETE FROM turns WHERE bot=?", [name])?);
            tx.commit()?;
            return Ok(out);
        }
        // Walk back from the head, freeing nodes until one is still reached
        // by another bot: as a head, a saved context start, or a parent of
        // a surviving branch. A fork's own suffix is what it leaves behind.
        // Nodes are small rows; a piece of them is a multiple of the turn piece.
        let mut node = bot.head;
        let mut freed = 0;
        while let Some(id) = node {
            if freed == piece * 32 {
                tx.execute("UPDATE bots SET head=? WHERE name=?", params![id, name])?;
                out["nodes"] = json!(freed);
                tx.commit()?;
                return Ok(out);
            }
            let referenced: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM bots WHERE name!=?2 AND (head=?1 OR context_start=?1 OR note=?1 OR compaction=?1 OR elision=?1))
                    OR EXISTS(SELECT 1 FROM nodes WHERE parent=?1)
                    OR EXISTS(SELECT 1 FROM notes WHERE previous=?1)
                    OR EXISTS(SELECT 1 FROM compactions WHERE previous=?1)
                    OR EXISTS(SELECT 1 FROM elisions WHERE previous=?1)",
                params![id, name],
                |r| r.get(0),
            )?;
            if referenced {
                break;
            }
            let parent: Option<i64> =
                tx.query_row("SELECT parent FROM nodes WHERE id=?", [id], |r| r.get(0))?;
            tx.execute("UPDATE bots SET head=? WHERE name=?", params![parent, name])?;
            tx.execute("DELETE FROM notes WHERE node=?", [id])?;
            tx.execute("DELETE FROM compactions WHERE node=?", [id])?;
            tx.execute("DELETE FROM elisions WHERE node=?", [id])?;
            tx.execute("DELETE FROM stubs WHERE node=?", [id])?;
            tx.execute("DELETE FROM nodes WHERE id=?", [id])?;
            freed += 1;
            node = parent;
        }
        tx.execute("DELETE FROM events WHERE bot=?", [name])?;
        tx.execute("DELETE FROM bots WHERE name=?", [name])?;
        out["nodes"] = json!(freed);
        out["done"] = json!(true);
        tx.commit()?;
        Ok(out)
    }
    /// Keep unfinished work and the preceding `keep_turns` turns' records.
    /// With no unfinished work, keep the newest `keep_turns`. Drop the rest of the
    /// bot's events, tool intents, processes, and artifacts. The transcript
    /// and the turn rows themselves stay: retention here bounds what replay
    /// and tool retrieval keep, never what the model said.
    pub fn prune(&mut self, name: &str, keep_turns: usize) -> Result<Value> {
        self.prune_except(name, keep_turns, None)
    }
    /// Retention as part of a completion: the turn ending in this job keeps
    /// its records, so its terminal event is published and replayable until
    /// the next pass. Live delivery is what the store holds, never more.
    pub fn prune_except(
        &mut self,
        name: &str,
        keep_turns: usize,
        protect: Option<i64>,
    ) -> Result<Value> {
        if !self.exists(name)? {
            return fail("bot_not_found");
        }
        self.prune_records(name, keep_turns, protect, 0, None)
    }
    /// One identity-bound piece: the records of up to `limit` prunable turns
    /// after `after`, oldest first. `next_after` names where the next piece
    /// starts, or is null when this one reached the retention boundary.
    pub fn prune_piece(
        &mut self,
        name: &str,
        expected_id: i64,
        keep_turns: usize,
        after: i64,
        limit: usize,
    ) -> Result<Value> {
        // Replace the existing existence read with an identity check. The
        // worker cannot interleave another mutation inside this storage job.
        let matches: bool = self
            .conn
            .prepare_cached("SELECT EXISTS(SELECT 1 FROM bots WHERE name=? AND id=?)")?
            .query_row(params![name, expected_id], |r| r.get(0))?;
        if !matches {
            return fail("bot_not_found");
        }
        self.prune_records(name, keep_turns, None, after, Some(limit))
    }
    /// Without a limit the whole prune is one piece, as completion needs.
    fn prune_records(
        &mut self,
        name: &str,
        keep_turns: usize,
        protect: Option<i64>,
        after: i64,
        limit: Option<usize>,
    ) -> Result<Value> {
        if keep_turns == 0 {
            return fail("invalid_retention");
        }
        let floor: Option<i64> = self
            .conn
            .prepare_cached(
                // Find the unfinished suffix through the active-status indexes,
                // then seek backwards in turns_bot_id. A large queued backlog
                // neither moves the retention boundary nor needs to be scanned.
                "SELECT id FROM turns WHERE bot=?1 AND id < COALESCE(
                    (SELECT MIN(id) FROM (
                        SELECT running_turn AS id FROM bots WHERE name=?1
                        UNION ALL SELECT MIN(id) FROM turns WHERE bot=?1 AND status='queued'
                        UNION ALL SELECT MIN(id) FROM turns WHERE bot=?1 AND status='ready'
                    )), 9223372036854775807)
                 ORDER BY id DESC LIMIT 1 OFFSET ?2",
            )?
            .query_row(params![name, (keep_turns - 1) as i64], |r| r.get(0))
            .optional()?;
        let Some(floor) = floor else {
            return Ok(json!({"events":0,"pruned_cursor":Value::Null,"next_after":Value::Null}));
        };
        let tx = self.conn.savepoint()?;
        // The candidate index contains only this bot's unpruned turns, not
        // its entire history or operational records owned by other bots.
        let turns: Vec<i64> = tx
            .prepare_cached(
                "SELECT turn FROM retained_turns WHERE bot=?1 AND turn<?2 AND turn IS NOT ?3 AND turn>?4
                 ORDER BY turn LIMIT ?5",
            )?
            .query_map(
                params![name, floor, protect, after, limit.map(|l| l as i64).unwrap_or(-1)],
                |r| r.get(0),
            )?
            .collect::<rusqlite::Result<_>>()?;
        let mut events = 0;
        let mut cursor: Option<i64> = None;
        for turn in &turns {
            // Background commands may outlive their launching turn; their
            // running row is required when the result commits and waiters wake.
            tx.prepare_cached("DELETE FROM artifacts WHERE turn=?")?
                .execute([turn])?;
            tx.prepare_cached("DELETE FROM processes WHERE turn=? AND status!='running'")?
                .execute([turn])?;
            tx.prepare_cached("DELETE FROM tools WHERE turn=?")?
                .execute([turn])?;
            let last: Option<i64> = tx
                .prepare_cached("SELECT MAX(id) FROM events WHERE turn=?")?
                .query_row([turn], |r| r.get(0))?;
            cursor = cursor.max(last);
            events += tx
                .prepare_cached("DELETE FROM events WHERE turn=?")?
                .execute([turn])?;
            // Keep pending background results discoverable by later prunes, even
            // after their launching turn's events and tool records are gone.
            tx.prepare_cached(
                "DELETE FROM retained_turns WHERE turn=?1
                 AND NOT EXISTS(SELECT 1 FROM processes WHERE turn=?1 AND status='running')",
            )?
            .execute([turn])?;
        }
        if let Some(cursor) = cursor {
            tx.prepare_cached(
                "UPDATE event_retention SET pruned_cursor=? WHERE singleton=1 AND pruned_cursor<?",
            )?
            .execute(params![cursor, cursor])?;
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
        let next_after = match limit {
            Some(limit) if turns.len() == limit => turns.last().copied(),
            _ => None,
        };
        Ok(json!({"events":events,"pruned_cursor":pruned,"next_after":next_after}))
    }
    /// Page immutable lineage metadata newest first, including a fork's shared prefix.
    /// `from` is inclusive; `next_from` is the parent to pass for the next page.
    pub fn history_nodes(
        &self,
        name: &str,
        from: Option<i64>,
        limit: usize,
        min_node: Option<i64>,
        oldest_first: bool,
    ) -> Result<Value> {
        if !(1..=400).contains(&limit) {
            return fail("invalid_history_limit");
        }
        let snapshot = self.conn.unchecked_transaction()?;
        let head = self.inspect(name)?.head;
        let mut next = from.or(head);
        let range_head = next;
        let floor = min_node.unwrap_or(0);
        if let Some(wanted) = from
            && !self.in_lineage(head, wanted)?
        {
            return fail("item_not_in_bot_history");
        }
        if oldest_first {
            // Node IDs increase along every lineage, including forks. Find the
            // first bounded page after the visible window on the read worker.
            let ids = self.conn.prepare_cached(
                "WITH RECURSIVE chain(id,parent) AS (
                    SELECT id,parent FROM nodes WHERE id=?1 AND id>=?2
                    UNION ALL SELECT n.id,n.parent FROM nodes n JOIN chain c ON n.id=c.parent WHERE n.id>=?2
                 ) SELECT id FROM chain ORDER BY id LIMIT ?3"
            )?.query_map(params![next, floor, limit as i64], |r| r.get::<_,i64>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            next = ids.last().copied();
        }
        let mut nodes = Vec::with_capacity(limit);
        let mut statement = self
            .conn
            .prepare_cached("SELECT parent,turn FROM nodes WHERE id=?")?;
        let mut unassigned = 0;
        while let Some(id) = next.filter(|id| *id >= floor) {
            let (parent, turn): (Option<i64>, Option<i64>) =
                statement.query_row([id], |r| Ok((r.get(0)?, r.get(1)?)))?;
            nodes.push(json!({"node":id,"turn":null}));
            if let Some(turn) = turn {
                for node in &mut nodes[unassigned..] {
                    node["turn"] = json!(turn);
                }
                unassigned = nodes.len();
            }
            next = parent;
            if nodes.len() == limit {
                break;
            }
        }
        // Only turn-start nodes carry a turn. Complete a page ending mid-turn by
        // finding its nearest start, including retained nodes of a deleted source.
        let mut ancestor = next;
        while unassigned < nodes.len() {
            let Some(id) = ancestor else {
                break;
            };
            let (parent, turn): (Option<i64>, Option<i64>) =
                statement.query_row([id], |r| Ok((r.get(0)?, r.get(1)?)))?;
            if let Some(turn) = turn {
                for node in &mut nodes[unassigned..] {
                    node["turn"] = json!(turn);
                }
                break;
            }
            ancestor = parent;
        }
        let next_newer = nodes
            .first()
            .and_then(|n| n["node"].as_i64())
            .filter(|id| Some(*id) != range_head)
            .map(|id| id + 1);
        snapshot.commit()?;
        Ok(
            json!({"nodes":nodes,"next_from":next.filter(|id| *id >= floor),"next_newer":next_newer}),
        )
    }
    /// Fetch a byte-bounded batch after one ancestry walk for all requested IDs.
    pub fn history_items(&self, name: &str, wanted: &[i64]) -> Result<Value> {
        use std::collections::HashSet;
        if wanted.is_empty() || wanted.len() > 400 {
            return fail("invalid_history_limit");
        }
        let unique: HashSet<i64> = wanted.iter().copied().collect();
        if unique.len() != wanted.len() {
            return fail("duplicate_history_node");
        }
        let snapshot = self.conn.unchecked_transaction()?;
        let head = self.inspect(name)?.head;
        let floor = *wanted.iter().min().unwrap();
        let ids = serde_json::to_string(wanted)?;
        let found: HashSet<i64> = self.conn.prepare_cached(
            "WITH RECURSIVE chain(id,parent) AS (
                SELECT id,parent FROM nodes WHERE id=?1
                UNION ALL SELECT n.id,n.parent FROM nodes n JOIN chain c ON n.id=c.parent WHERE n.id>=?2
             ) SELECT id FROM chain WHERE id IN (SELECT value FROM json_each(?3))"
        )?.query_map(params![head,floor,ids], |row| row.get(0))?.collect::<rusqlite::Result<_>>()?;
        if found != unique {
            return fail("item_not_in_bot_history");
        }
        let mut items = Vec::new();
        let mut bytes = 0;
        let maximum = crate::output::MAX_EVENT - 1024;
        // Check the encoded blob length in SQLite before allocating it. An
        // unrenderable item gets its own error; adjacent items remain readable.
        let mut query = self
            .conn
            .prepare_cached("SELECT item FROM nodes WHERE id=? AND length(item)<=?")?;
        for &node in wanted {
            let raw: Option<Vec<u8>> = query
                .query_row(params![node, maximum as i64], |r| r.get(0))
                .optional()?;
            let mut entry = match raw {
                Some(raw) => json!({"node":node,"item":serde_json::from_slice::<Value>(&raw)?}),
                None => json!({"node":node,"error":"item_too_large"}),
            };
            let mut size = serde_json::to_vec(&entry)?.len();
            // Leave room for the protocol envelope, including extra escaping
            // or wrapper bytes beyond the stored representation.
            if size > maximum {
                entry = json!({"node":node,"error":"item_too_large"});
                size = serde_json::to_vec(&entry)?.len();
            }
            if !items.is_empty() && bytes + size > 768 * 1024 {
                break;
            }
            bytes += size;
            items.push(entry);
        }
        snapshot.commit()?;
        Ok(json!({"items":items}))
    }
    pub fn item(&self, name: &str, wanted: i64) -> Result<Value> {
        let snapshot = self.conn.unchecked_transaction()?;
        let head = self.inspect(name)?.head;
        if !self.in_lineage(head, wanted)? {
            return fail("item_not_in_bot_history");
        }
        let item: Vec<u8> =
            self.conn
                .query_row("SELECT item FROM nodes WHERE id=?", [wanted], |r| r.get(0))?;
        snapshot.commit()?;
        Ok(serde_json::from_slice(&item)?)
    }
    /// A bot's turns in id order, paged by `after`, with accounting a program
    /// needs without replaying events.
    /// Flush an execution segment's retry and pacing accounting once when it
    /// finishes, is interrupted, or parks; zero counters need no write.
    /// Counts a controller reads instead of scanning: parked turns and
    /// running background commands, both from the active-status indexes.
    /// Waiting turns, running processes, turns queued or ready to start,
    /// and turns paced on a closed pool.
    pub fn counts(&self) -> Result<(i64, i64, i64, i64)> {
        let waiting: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM turns WHERE status='waiting'",
            [],
            |r| r.get(0),
        )?;
        let paced: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM turns WHERE status='paced'", [], |r| {
                    r.get(0)
                })?;
        let (queued, _) = self.pending()?;
        Ok((waiting, self.running_processes()?, queued, paced))
    }
    pub fn note_pacing(&mut self, turn: i64, retries: u64, paced_ms: u64) -> Result<()> {
        self.conn.execute(
            "UPDATE turns SET retries=retries+?,paced_ms=paced_ms+? WHERE id=?",
            params![retries as i64, paced_ms as i64, turn],
        )?;
        Ok(())
    }
    pub fn turns(&self, name: &str, after: i64, limit: usize) -> Result<Value> {
        self.inspect(name)?;
        if after < 0 || !(1..=256).contains(&limit) {
            return fail("invalid_turn_page");
        }
        let mut statement = self.conn.prepare(
            "SELECT t.id,t.request_id,t.status,COALESCE(t.workspace,b.workspace),
                    COALESCE(t.model,b.provider||'/'||b.model),t.input_tokens,t.output_tokens,
                    t.model_rounds,t.started_ms,t.finished_ms,
                    substr(CASE WHEN t.prompt_node IS NULL THEN t.prompt ELSE json_extract(CAST(n.item AS TEXT),'$.content[0].text') END,1,200),
                    length(CASE WHEN t.prompt_node IS NULL THEN t.prompt ELSE json_extract(CAST(n.item AS TEXT),'$.content[0].text') END),
                    t.retries,t.paced_ms,t.delivery,t.cached_input_tokens
             FROM turns t JOIN bots b ON b.name=t.bot LEFT JOIN nodes n ON n.id=t.prompt_node
             WHERE t.bot=? AND t.id>? ORDER BY t.id LIMIT ?",
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
                "prompt_preview":preview,"prompt_bytes":r.get::<_, i64>(11)?,
                "retries":r.get::<_, i64>(12)?,"paced_ms":r.get::<_, i64>(13)?,
                "delivery":r.get::<_, String>(14)?,
                "cached_input_tokens":r.get::<_, i64>(15)?,
                "cache_hit":cache_hit(r.get::<_, i64>(15)?, r.get::<_, i64>(5)?)}),
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
        // Retention removed the turn's events with its artifacts, but the
        // transcript keeps the nodes: a branch that inherited the output is
        // told the artifact is gone, not that the turn is somebody else's.
        if let (Some(Some(head)), None) = (head, node)
            && self.pruned(turn)?
            && self.lineage_holds_output(head, turn, call_id)?
        {
            return fail("artifact_pruned");
        }
        fail("turn_not_found")
    }
    /// An existing turn keeps at least its start event until retention
    /// removes them together with its artifacts.
    fn pruned(&self, turn: i64) -> Result<bool> {
        Ok(!self
            .conn
            .prepare_cached("SELECT 1 FROM events WHERE turn=? LIMIT 1")?
            .exists([turn])?)
    }
    /// Whether the tool result of `call_id` in `turn` is on the chain ending
    /// at `head`. Only a turn's prompt node records its turn, and ids grow
    /// along a chain, so the turn's nodes are those between its prompt and
    /// the next prompt; the walk stops at the turn's own prompt.
    fn lineage_holds_output(&self, head: i64, turn: i64, call_id: &str) -> Result<bool> {
        Ok(self
            .conn
            .prepare_cached(
                "WITH RECURSIVE chain(id,parent,turn) AS (
                    SELECT id,parent,turn FROM nodes WHERE id=?1
                    UNION ALL SELECT n.id,n.parent,n.turn FROM nodes n JOIN chain c ON n.id=c.parent
                    WHERE c.turn IS NULL OR c.turn>?2)
                 SELECT 1 FROM chain c JOIN nodes n ON n.id=c.id
                 WHERE c.id>(SELECT id FROM chain WHERE turn=?2)
                   AND c.id<COALESCE((SELECT MIN(id) FROM chain WHERE turn>?2),9223372036854775807)
                   AND ((json_extract(CAST(n.item AS TEXT),'$.type')='function_call_output'
                         AND json_extract(CAST(n.item AS TEXT),'$.call_id')=?3)
                     OR EXISTS(SELECT 1 FROM json_each(CAST(n.item AS TEXT),'$.content')
                        WHERE json_extract(value,'$.type')='tool_result'
                        AND json_extract(value,'$.tool_use_id')=?3)) LIMIT 1",
            )?
            .exists(params![head, turn, call_id])?)
    }
    /// The turn is the caller's to read, and no stream is stored for the call.
    fn missing_artifact<T>(&self, turn: i64) -> Result<T> {
        if self.pruned(turn)? {
            fail("artifact_pruned")
        } else {
            fail("artifact_not_found")
        }
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
        let Some((_, data)) = artifact::read(&self.conn, turn, call_id, stream, 0, usize::MAX)?
        else {
            return self.missing_artifact(turn);
        };
        crate::tools::page_lines(&String::from_utf8_lossy(&data), offset, limit)
    }
    /// A recorded tool result on the bot's own lineage, as text, for the
    /// model's own `read` of what an elided result's stub names.
    pub fn result_lines(
        &self,
        name: &str,
        node: i64,
        offset: usize,
        limit: usize,
    ) -> Result<String> {
        if offset == 0 || !(1..=5000).contains(&limit) {
            return fail("invalid_tool_arguments");
        }
        let head: Option<Option<i64>> = self
            .conn
            .query_row("SELECT head FROM bots WHERE name=?", [name], |r| r.get(0))
            .optional()?;
        if !self.in_lineage(head.flatten(), node)? {
            return fail("result_not_found");
        }
        let item: Vec<u8> =
            self.conn
                .query_row("SELECT item FROM nodes WHERE id=?", [node], |r| r.get(0))?;
        let Some((_, _, output)) = super::context::tool_result(&item) else {
            return fail("result_not_found");
        };
        crate::tools::page_pieces(&output, offset, limit)
    }
    pub fn artifact(&self, name: &str, turn: i64, call_id: &str) -> Result<Value> {
        self.authorize_artifact(name, turn, call_id)?;
        let mut statement = self
            .conn
            .prepare("SELECT stream FROM artifacts WHERE turn=? AND call_id=?")?;
        let mut rows = statement.query(params![turn, call_id])?;
        let mut streams = serde_json::Map::new();
        while let Some(row) = rows.next()? {
            let stream: String = row.get(0)?;
            let (_, data) = artifact::read(&self.conn, turn, call_id, &stream, 0, usize::MAX)?
                .ok_or_else(|| Error::new("storage_error"))?;
            streams.insert(
                stream,
                Value::String(String::from_utf8_lossy(&data).into_owned()),
            );
        }
        if streams.is_empty() {
            return self.missing_artifact(turn);
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
        let Some((total, bytes)) =
            artifact::read(&self.conn, turn, call_id, stream, offset, limit)?
        else {
            return self.missing_artifact(turn);
        };
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
    record_usage_for(conn, bot, turn, usage, None)
}
fn record_usage_for(
    conn: &Connection,
    bot: &str,
    turn: i64,
    usage: &Usage,
    purpose: Option<&str>,
) -> Result<Value> {
    let mut data = serde_json::to_value(usage)?;
    if let Some(purpose) = purpose {
        data["purpose"] = json!(purpose);
    }
    let cursor = event(conn, bot, Some(turn), "usage", data.clone())?;
    conn.execute(
        "UPDATE turns SET input_tokens=input_tokens+?,output_tokens=output_tokens+?,
             cached_input_tokens=cached_input_tokens+? WHERE id=?",
        params![
            usage.input_tokens as i64,
            usage.output_tokens as i64,
            usage.cached_input_tokens as i64,
            turn
        ],
    )?;
    conn.execute(
        "UPDATE bots SET tokens_used=tokens_used+?,input_tokens=input_tokens+?,
             cached_input_tokens=cached_input_tokens+? WHERE name=?",
        params![
            usage.input_tokens.saturating_add(usage.output_tokens) as i64,
            usage.input_tokens as i64,
            usage.cached_input_tokens as i64,
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
/// Put a turn's user item on the lineage and mark the bot busy with it.
fn start_locked(tx: &Connection, bot: &Bot, turn: i64, prompt: &str) -> Result<Option<i64>> {
    let item = bot.family()?.user_item(prompt)?;
    let head = node_with_turn(tx, bot.head, &item, Some(turn))?;
    if prompt.len() >= PROMPT_SHARE_BYTES {
        tx.execute(
            "UPDATE turns SET status='running',started_ms=?,prompt='',prompt_node=? WHERE id=?",
            params![epoch_ms(), head, turn],
        )?;
    } else {
        tx.execute(
            "UPDATE turns SET status='running',started_ms=? WHERE id=?",
            params![epoch_ms(), turn],
        )?;
    }
    tx.execute(
        "UPDATE bots SET head=?,status='running',running_turn=? WHERE name=?",
        params![head, turn, bot.name],
    )?;
    Ok(Some(head))
}
/// The bot's oldest queued turn now waits only for a slot. True when that
/// turn was a steer, for the queued-steer count.
fn promote(tx: &Connection, bot: &str) -> Result<bool> {
    let delivery: Option<String> = tx
        .prepare_cached(
            "UPDATE turns SET status='ready' WHERE id=(
                SELECT MIN(id) FROM turns WHERE bot=? AND status='queued') RETURNING delivery",
        )?
        .query_row([bot], |r| r.get(0))
        .optional()?;
    Ok(delivery.as_deref() == Some("steer"))
}
fn turn_status_name(status: &str) -> &'static str {
    match status {
        "queued" => "queued",
        "ready" => "ready",
        "waiting" => "waiting",
        "paced" => "paced",
        "running" => "running",
        _ => "finished",
    }
}
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
    if from < 13 {
        // 12 -> 13: retries and pacing delay per turn. Added only when
        // missing, so a store whose version was reset keeps working.
        let present: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('turns')")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for column in ["retries", "paced_ms"] {
            if !present.iter().any(|c| c == column) {
                conn.execute_batch(&format!(
                    "ALTER TABLE turns ADD COLUMN {column} INTEGER NOT NULL DEFAULT 0;"
                ))?;
            }
        }
    }
    if from < 19 {
        // 18 -> 19: cached input tokens per turn and per bot, for the hit ratio.
        let mut added = false;
        for (table, column) in [
            ("turns", "cached_input_tokens"),
            ("bots", "input_tokens"),
            ("bots", "cached_input_tokens"),
        ] {
            let present: bool = conn.query_row(
                &format!(
                    "SELECT EXISTS(SELECT 1 FROM pragma_table_info('{table}') WHERE name='{column}')"
                ),
                [],
                |r| r.get(0),
            )?;
            if !present {
                added = true;
                conn.execute_batch(&format!(
                    "ALTER TABLE {table} ADD COLUMN {column} INTEGER NOT NULL DEFAULT 0;"
                ))?;
            }
        }
        if added {
            migrate_cache_usage(conn)?;
        }
    }
    if from < 18 {
        // 17 -> 18: tools are chosen per bot. Earlier stores did not retain
        // that choice, so only an empty store can be converted without guessing.
        let present: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('bots') WHERE name='tools')",
            [],
            |r| r.get(0),
        )?;
        if !present {
            let has_bots: bool =
                conn.query_row("SELECT EXISTS(SELECT 1 FROM bots)", [], |r| r.get(0))?;
            if has_bots {
                return fail_with(
                    "store_migration_tools_unknown",
                    "existing bots have no recorded tool selection; keep this store and use a new store path",
                );
            }
            conn.execute_batch("ALTER TABLE bots ADD COLUMN tools TEXT NOT NULL;")?;
        }
    }
    if from < 17 {
        // 16 -> 17: strict steers name the turn they are for.
        let present: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('turns') WHERE name='expected_turn')",
            [],
            |r| r.get(0),
        )?;
        if !present {
            conn.execute_batch("ALTER TABLE turns ADD COLUMN expected_turn INTEGER;")?;
        }
    }
    if from < 16 {
        // 15 -> 16: the store no longer binds a provider set or toolset.
        // A bot's provider is checked by family when a turn starts.
        conn.execute_batch("DROP TABLE IF EXISTS configuration;")?;
    }
    if from < 15 {
        // 14 -> 15: delivery mode per turn. Added only when missing.
        let present: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('turns') WHERE name='delivery')",
            [],
            |r| r.get(0),
        )?;
        if !present {
            conn.execute_batch(
                "ALTER TABLE turns ADD COLUMN delivery TEXT NOT NULL DEFAULT 'reject';",
            )?;
        }
    }
    if from < 14 {
        // Seed a durable global watermark, including holes left by deleted
        // bots. This indexed scan runs once during migration, never on replay.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS event_retention(singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                pruned_cursor INTEGER NOT NULL);
             INSERT OR IGNORE INTO event_retention SELECT 1, MAX(
                COALESCE((SELECT MAX(pruned_cursor) FROM bots),0),
                COALESCE((SELECT MAX(e.id-1) FROM events e WHERE e.id>1
                    AND NOT EXISTS(SELECT 1 FROM events p WHERE p.id=e.id-1)),0),
                CASE WHEN COALESCE((SELECT seq FROM sqlite_sequence WHERE name='events'),0)
                    > COALESCE((SELECT MAX(id) FROM events),0)
                    THEN (SELECT seq FROM sqlite_sequence WHERE name='events') ELSE 0 END);",
        )?;
    }
    if from < 20 {
        // Retire the old blocked state once. Stage its unfinished turn for
        // normal startup reconciliation, which appends missing results and
        // releases the bot. Persisting this staging makes recovery retryable
        // if the daemon dies between migration and turn finalization.
        let blocked: Vec<(String, Option<i64>)> = conn
            .prepare("SELECT name,(SELECT id FROM turns WHERE bot=bots.name AND status='uncertain' ORDER BY id DESC LIMIT 1) FROM bots WHERE status='uncertain' AND running_turn IS NULL")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (bot, turn) in blocked {
            let Some(turn) = turn else {
                return fail("store_migration_missing_uncertain_turn");
            };
            migrate_unanswered_tools(conn, &bot, turn)?;
            conn.execute(
                "UPDATE bots SET status='running',running_turn=? WHERE name=?",
                params![turn, bot],
            )?;
            conn.execute("UPDATE turns SET status='running' WHERE id=?", [turn])?;
        }
    }
    if from < 21 {
        // 20 -> 21: bots gain a store-wide identity that is never reused.
        // Existing positive rowids give creation order without counting each
        // prefix again. Preserve gaps left by deletion and seed the sequence
        // above the largest assigned id, not the number of surviving bots.
        // Added only when missing, so a reset version keeps its identities.
        let present: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('bots') WHERE name='id')",
            [],
            |r| r.get(0),
        )?;
        if !present {
            conn.execute_batch(
                "ALTER TABLE bots ADD COLUMN id INTEGER;
                 UPDATE bots SET id=rowid;
                 CREATE TABLE IF NOT EXISTS bot_sequence(singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                    last_id INTEGER NOT NULL CHECK(last_id>=0));
                 INSERT OR REPLACE INTO bot_sequence SELECT 1,COALESCE(MAX(id),0) FROM bots;",
            )?;
        }
    }
    // Version 25 joins the independently published lineage (22/23) and
    // compaction (22/23/24) schemas. Inspect columns once during migration
    // so either branch retains its data and receives only the missing fields.
    for (column, kind) in [("created_by", "TEXT"), ("created_by_id", "INTEGER")] {
        let present: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('bots') WHERE name=?)",
            [column],
            |r| r.get(0),
        )?;
        if !present {
            conn.execute_batch(&format!("ALTER TABLE bots ADD COLUMN {column} {kind};"))?;
        }
    }

    {
        // 21 -> 22: carry-forward notes, versioned by the node of the tool
        // result that wrote them. Added only when missing.
        let present: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('bots') WHERE name='note')",
            [],
            |r| r.get(0),
        )?;
        if !present {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS notes(node INTEGER PRIMARY KEY REFERENCES nodes(id),
                    previous INTEGER REFERENCES notes(node), text TEXT NOT NULL);
                 ALTER TABLE bots ADD COLUMN note INTEGER REFERENCES notes(node);",
            )?;
        }
    }

    {
        // 22 -> 23: compaction, versioned by the cut node, with the client's
        // instructions and summarizer per bot. Added only when missing.
        let present: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('bots') WHERE name='compaction')",
            [],
            |r| r.get(0),
        )?;
        if !present {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS compactions(node INTEGER PRIMARY KEY REFERENCES nodes(id),
                    previous INTEGER REFERENCES compactions(node), summary TEXT NOT NULL, prompts TEXT NOT NULL,
                    covered_from INTEGER NOT NULL, covered_to INTEGER NOT NULL);
                 ALTER TABLE bots ADD COLUMN compaction INTEGER REFERENCES compactions(node);
                 ALTER TABLE bots ADD COLUMN compaction_instructions TEXT;
                 ALTER TABLE bots ADD COLUMN compaction_model TEXT;",
            )?;
        }
    }

    if !conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('compactions') WHERE name='cut')",
        [],
        |r| r.get::<_, bool>(0),
    )? {
        conn.execute_batch(
            "ALTER TABLE compactions ADD COLUMN cut INTEGER REFERENCES nodes(id);
             UPDATE compactions SET cut=node;",
        )?;
    }

    // 25 -> 26: share a started turn's prompt with its immutable user node.
    // Pending/cancelled-before-start turns still own inline text. Older steers
    // without an indexed prompt node retain their exact inline copy as well.
    if !conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('turns') WHERE name='prompt_node')",
        [],
        |r| r.get::<_, bool>(0),
    )? {
        conn.execute_batch(
            "CREATE UNIQUE INDEX IF NOT EXISTS nodes_turn ON nodes(turn) WHERE turn IS NOT NULL;
            ALTER TABLE turns ADD COLUMN prompt_node INTEGER REFERENCES nodes(id);
            UPDATE turns SET prompt_node=(SELECT n.id FROM nodes n WHERE n.turn=turns.id
                AND json_extract(CAST(n.item AS TEXT),'$.role')='user'
                AND json_extract(CAST(n.item AS TEXT),'$.content[0].text')=turns.prompt)
                WHERE length(CAST(prompt AS BLOB))>=4096;
            UPDATE turns SET prompt='' WHERE prompt_node IS NOT NULL;",
        )?;
    }
    if !conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('artifacts') WHERE name='raw_bytes')",
        [],
        |r| r.get::<_, bool>(0),
    )? {
        // Existing BLOBs stay raw. New large artifacts may use bounded LZ4 blocks.
        conn.execute_batch(
            "ALTER TABLE artifacts ADD COLUMN raw_bytes INTEGER NOT NULL DEFAULT 0;",
        )?;
    }
    if !conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('bots') WHERE name='cache_bot')",
        [],
        |r| r.get::<_, bool>(0),
    )? {
        // 26 -> 27: forks share their source's prompt cache key. Existing
        // bots keep their own, which the daemon renews at each start anyway.
        conn.execute_batch("ALTER TABLE bots ADD COLUMN cache_bot INTEGER;")?;
    }
    if !conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('bots') WHERE name='fallbacks')",
        [],
        |r| r.get::<_, bool>(0),
    )? {
        // 27 -> 28: server-side fallbacks become a bot's choice, off unless
        // asked for, and the store draws an identity once for cache keys.
        conn.execute_batch(
            "ALTER TABLE bots ADD COLUMN fallbacks INTEGER NOT NULL DEFAULT 0;
             CREATE TABLE IF NOT EXISTS store(singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                identity INTEGER NOT NULL);
             INSERT OR IGNORE INTO store VALUES (1,random());",
        )?;
    }
    if !conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('nodes') WHERE name='thinking')",
        [],
        |r| r.get::<_, bool>(0),
    )? {
        // 26 -> 27: what each stored item loses without its thinking, and
        // per bot where thinking still matches its context. Existing bots
        // have no fingerprint, so their first request sends older thinking
        // without it once rather than risk a block bound elsewhere.
        conn.execute_batch(
            "ALTER TABLE nodes ADD COLUMN thinking INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE bots ADD COLUMN thinking_prefix INTEGER;
             ALTER TABLE bots ADD COLUMN thinking_floor INTEGER NOT NULL DEFAULT 0;",
        )?;
        let mut update = conn.prepare("UPDATE nodes SET thinking=? WHERE id=?")?;
        let mut select = conn
            .prepare("SELECT id,item FROM nodes WHERE instr(item,CAST('thinking\"' AS BLOB))>0")?;
        let mut rows = select.query([])?;
        while let Some(row) = rows.next()? {
            let item: Vec<u8> = row.get(1)?;
            let bytes = super::thinking_bytes(&item);
            if bytes > 0 {
                update.execute(params![bytes as i64, row.get::<_, i64>(0)?])?;
            }
        }
    }
    if !conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('bots') WHERE name='elision')",
        [],
        |r| r.get::<_, bool>(0),
    )? {
        // 28 -> 29: tool result stubs and versioned elision floors, schema
        // only. Results stored before have no stub and are always sent
        // whole: a backfilled saving would change the cumulative total of
        // every later node on its lineage, rewriting most of the store.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS stubs(node INTEGER PRIMARY KEY REFERENCES nodes(id),
                item BLOB NOT NULL);
             CREATE TABLE IF NOT EXISTS elisions(node INTEGER PRIMARY KEY REFERENCES nodes(id),
                previous INTEGER REFERENCES elisions(node), through INTEGER NOT NULL,
                saved INTEGER NOT NULL);
             ALTER TABLE bots ADD COLUMN elision INTEGER REFERENCES elisions(node);
             ALTER TABLE nodes ADD COLUMN elided INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE nodes ADD COLUMN total_elided INTEGER NOT NULL DEFAULT 0;",
        )?;
    }
    Ok(())
}
/// Keep the oldest and newest excerpts within the text/metadata budget.
/// Locate the same middle range that repeated removals would discard, then
/// drain once: trimming a large batch moves the retained suffix only once.
fn bound_prompts(prompts: &mut Vec<(i64, String)>) {
    let cost = |prompt: &(i64, String)| std::mem::size_of::<(i64, String)>() + prompt.1.len();
    let mut total: usize = prompts.iter().map(cost).sum();
    let (mut left, mut right) = (prompts.len() / 2, prompts.len() / 2);
    while total > Database::COMPACTION_PROMPTS_BYTES && prompts.len() - (right - left) > 2 {
        let middle = (prompts.len() - (right - left)) / 2;
        let removed = if middle < left {
            left -= 1;
            left
        } else {
            right += 1;
            right - 1
        };
        total -= cost(&prompts[removed]);
    }
    prompts.drain(left..right);
}

/// A prompt kept verbatim by a compaction, cut to `limit` bytes at a
/// character boundary with an ellipsis when anything was left out.
fn bounded_prompt(prompt: &str, limit: usize) -> String {
    if prompt.len() <= limit {
        return prompt.to_owned();
    }
    let mut end = limit;
    while !prompt.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &prompt[..end])
}
/// The first line of a prompt, cut to `limit` bytes at a character boundary,
/// with an ellipsis when anything was left out.
fn first_line(prompt: &str, limit: usize) -> String {
    let line = prompt.lines().next().unwrap_or("").trim();
    if line.len() <= limit && prompt.lines().nth(1).is_none() {
        return line.to_owned();
    }
    let mut end = limit.min(line.len());
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", line[..end].trim_end())
}
/// Allocate the next bot identity inside the caller's transaction.
fn identity(conn: &Connection) -> Result<i64> {
    Ok(conn
        .prepare_cached(
            "UPDATE bot_sequence SET last_id=last_id+1 WHERE singleton=1 RETURNING last_id",
        )?
        .query_row([], |r| r.get(0))?)
}

/// Operational tool records can have been pruned after a blocked turn's
/// queued successors failed. Reconstruct missing intents from its durable
/// transcript, one item at a time. With no retained intent, execution is unknown.
fn migrate_unanswered_tools(conn: &Connection, bot: &str, turn: i64) -> Result<()> {
    let mut next: Option<i64> =
        conn.query_row("SELECT head FROM bots WHERE name=?", [bot], |r| r.get(0))?;
    let mut read = conn.prepare("SELECT parent,item,turn FROM nodes WHERE id=?")?;
    let mut insert =
        conn.prepare("INSERT OR IGNORE INTO tools(turn,call_id,status) VALUES (?,?,'executing')")?;
    let mut answered = std::collections::HashSet::new();
    while let Some(id) = next {
        let (parent, raw, marker): (Option<i64>, Vec<u8>, Option<i64>) =
            read.query_row([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        let item: Value = serde_json::from_slice(&raw)?;
        let blocks: Vec<&Value> = if item["type"].is_string() {
            vec![&item]
        } else {
            item["content"]
                .as_array()
                .into_iter()
                .flatten()
                .rev()
                .collect()
        };
        for block in blocks {
            let (call, result) = match block["type"].as_str() {
                Some("function_call") => (block["call_id"].as_str(), false),
                Some("tool_use") => (block["id"].as_str(), false),
                Some("function_call_output") => (block["call_id"].as_str(), true),
                Some("tool_result") => (block["tool_use_id"].as_str(), true),
                _ => (None, false),
            };
            if let Some(call) = call {
                if result {
                    answered.insert(call.to_owned());
                } else if !answered.remove(call) {
                    insert.execute(params![turn, call])?;
                }
            }
        }
        if marker == Some(turn) {
            break;
        }
        next = parent;
    }
    conn.execute(
        "INSERT OR IGNORE INTO retained_turns(turn,bot) VALUES (?,?)",
        params![turn, bot],
    )?;
    Ok(())
}

/// Backfill once at open, streaming usage events through the turn/kind index.
/// Missing retained usage is unknown, not a cache miss. The caller's opening
/// transaction rolls back both these updates and the new columns on failure.
fn migrate_cache_usage(conn: &Connection) -> Result<()> {
    let unavailable = || {
        Error::with(
            "store_migration_usage_unavailable",
            "retained usage cannot reconstruct cache totals; keep this store and use a new store path",
        )
    };
    let mut turns = conn.prepare("SELECT id,input_tokens,output_tokens FROM turns")?;
    let mut events = conn.prepare("SELECT data FROM events WHERE turn=? AND kind='usage'")?;
    let mut update = conn.prepare("UPDATE turns SET cached_input_tokens=? WHERE id=?")?;
    let mut rows = turns.query([])?;
    while let Some(row) = rows.next()? {
        let (turn, input, output): (i64, i64, i64) = (row.get(0)?, row.get(1)?, row.get(2)?);
        let mut usage = events.query([turn])?;
        let (mut sent, mut received, mut cached) = (0i64, 0i64, 0i64);
        while let Some(event) = usage.next()? {
            let data: Value =
                serde_json::from_str(&event.get::<_, String>(0)?).map_err(|_| unavailable())?;
            let read = |key: &str| {
                data[key]
                    .as_i64()
                    .filter(|n| *n >= 0)
                    .ok_or_else(unavailable)
            };
            let (i, o, c) = (
                read("input_tokens")?,
                read("output_tokens")?,
                read("cached_input_tokens")?,
            );
            if c > i {
                return Err(unavailable());
            }
            sent = sent.checked_add(i).ok_or_else(unavailable)?;
            received = received.checked_add(o).ok_or_else(unavailable)?;
            cached = cached.checked_add(c).ok_or_else(unavailable)?;
        }
        if (sent, received) != (input, output) {
            return Err(unavailable());
        }
        update.execute(params![cached, turn])?;
    }
    // Turns survive pruning, and forked bots own only their own usage.
    let mut bots = conn.prepare(
        "SELECT b.name,b.tokens_used,COALESCE(SUM(t.input_tokens),0),
            COALESCE(SUM(t.cached_input_tokens),0),COALESCE(SUM(t.input_tokens+t.output_tokens),0)
         FROM bots b LEFT JOIN turns t ON t.bot=b.name GROUP BY b.name",
    )?;
    let mut update =
        conn.prepare("UPDATE bots SET input_tokens=?,cached_input_tokens=? WHERE name=?")?;
    let mut rows = bots.query([])?;
    while let Some(row) = rows.next()? {
        if row.get::<_, i64>(1)? != row.get::<_, i64>(4)? {
            return Err(unavailable());
        }
        update.execute(params![
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, String>(0)?
        ])?;
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
    insert_node(conn, parent, item, turn, 0)
}
/// The id the next node insert takes, inside the caller's transaction.
fn next_node(conn: &Connection) -> Result<i64> {
    Ok(conn
        .prepare_cached(
            "SELECT MAX(last_id,COALESCE((SELECT MAX(id) FROM nodes),0))+1
             FROM node_sequence WHERE singleton=1",
        )?
        .query_row([], |r| r.get(0))?)
}
/// `elided`: what a request saves by sending the item's stub instead.
fn insert_node(
    conn: &Connection,
    parent: Option<i64>,
    item: &[u8],
    turn: Option<i64>,
    elided: i64,
) -> Result<i64> {
    let (bytes, depth, saved): (i64, i64, i64) = match parent {
        Some(id) => conn
            .prepare_cached("SELECT total_bytes,depth,total_elided FROM nodes WHERE id=?")?
            .query_row([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?,
        None => (0, 0, 0),
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
        "INSERT INTO nodes(id,parent,item,total_bytes,depth,turn,turn_seq,thinking,elided,total_elided)
         SELECT MAX(last_id,COALESCE((SELECT MAX(id) FROM nodes),0))+1,?,?,?,?,?,?,?,?,?
         FROM node_sequence WHERE singleton=1",
    )?
    .execute(params![
        parent,
        item,
        bytes + item.len() as i64,
        depth + 1,
        turn,
        turn_seq,
        super::thinking_bytes(item) as i64,
        elided,
        saved + elided
    ])?;
    Ok(conn.last_insert_rowid())
}

// Last in the file: bench.query_plans audits the statements above the first
// test marker.
#[cfg(test)]
impl Database {
    /// The connection itself, for tests that stage what no store method does.
    pub(crate) fn connection(&mut self) -> &mut Connection {
        &mut self.conn
    }
    /// One integer pragma as this connection has it.
    pub(crate) fn pragma(&self, name: &str) -> i64 {
        self.conn
            .pragma_query_value(None, name, |r| r.get(0))
            .unwrap()
    }
}
