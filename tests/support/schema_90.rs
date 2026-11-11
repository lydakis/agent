//! Stores in the shape schema 90 gave them, for the migration tests. Each
//! test crate uses what it needs of it.
#![allow(dead_code)]

use rusqlite::Connection;

/// Puts a store back in schema 90's shape, where a message and the turn it
/// opened were one `turns` row: the shape every store before 91 had, so an
/// older store built from today's is one the migrations really met.
pub const UNSPLIT: &str = "
    PRAGMA foreign_keys=OFF;
    CREATE TABLE turns_whole(id INTEGER PRIMARY KEY, bot TEXT NOT NULL REFERENCES bots(name),
        request_id TEXT NOT NULL, prompt TEXT NOT NULL, status TEXT NOT NULL,
        prompt_node INTEGER REFERENCES nodes(id),
        workspace TEXT, model TEXT, waiting TEXT, model_rounds INTEGER NOT NULL DEFAULT 0, view_sent INTEGER,
        input_tokens INTEGER NOT NULL DEFAULT 0, output_tokens INTEGER NOT NULL DEFAULT 0,
        started_ms INTEGER, finished_ms INTEGER,
        retries INTEGER NOT NULL DEFAULT 0, paced_ms INTEGER NOT NULL DEFAULT 0,
        summary_ms INTEGER NOT NULL DEFAULT 0, budget_tokens INTEGER,
        delivery TEXT NOT NULL DEFAULT 'reject', expected_turn INTEGER,
        cached_input_tokens INTEGER NOT NULL DEFAULT 0,
        from_bot TEXT, from_turn INTEGER, from_id INTEGER, origin TEXT, reasoning TEXT, inherited_reasoning TEXT,
        bot_model TEXT, bot_reasoning TEXT, images TEXT, person TEXT, quotes TEXT, reply_node INTEGER,
        wait_ms INTEGER, due_ms INTEGER, held TEXT,
        work INTEGER NOT NULL DEFAULT 0, limited TEXT, budget_counts TEXT, output_schema TEXT,
        drops INTEGER NOT NULL DEFAULT 0,
        UNIQUE(bot,request_id));
    INSERT INTO turns_whole(id,bot,request_id,prompt,status,prompt_node,workspace,model,waiting,
            model_rounds,view_sent,input_tokens,output_tokens,started_ms,finished_ms,retries,
            paced_ms,summary_ms,budget_tokens,delivery,expected_turn,cached_input_tokens,
            from_bot,from_turn,from_id,origin,reasoning,bot_model,bot_reasoning,images,person,
            quotes,reply_node,wait_ms,due_ms,held,work,limited,budget_counts,output_schema,drops)
        SELECT m.id,m.bot,m.request_id,m.text,
            CASE WHEN t.id IS NOT NULL THEN t.status WHEN m.status='taken' THEN 'steered'
                 WHEN m.status='withdrawn' THEN 'interrupted' ELSE m.status END,
            CASE WHEN m.status='taken' AND m.text='' THEN m.node END,
            COALESCE(t.workspace,m.workspace),COALESCE(t.model,m.model),t.waiting,
            COALESCE(t.model_rounds,0),t.view_sent,COALESCE(t.input_tokens,0),
            COALESCE(t.output_tokens,0),t.started_ms,
            COALESCE(t.finished_ms,CASE WHEN m.status='taken' THEN m.taken_ms END,
                (SELECT e.written_ms FROM events e WHERE e.message=m.id AND e.kind='message_ended')),
            COALESCE(t.retries,0),COALESCE(t.paced_ms,0),COALESCE(t.summary_ms,0),
            COALESCE(t.budget_tokens,m.budget_tokens),m.delivery,m.expected_turn,
            COALESCE(t.cached_input_tokens,0),m.from_bot,m.from_turn,m.from_id,m.origin,
            COALESCE(t.reasoning,m.reasoning),t.bot_model,t.bot_reasoning,m.images,m.person,
            (SELECT json_group_array(json(CASE WHEN json_type(q.value,'$.message') IS NULL THEN q.value
                ELSE json_remove(json_set(q.value,'$.turn',json_extract(q.value,'$.message')),'$.message')
                END)) FROM json_each(m.quotes) q),
            t.reply_node,m.wait_ms,m.due_ms,m.held,COALESCE(t.work,0),t.limited,
            COALESCE(t.budget_counts,m.budget_counts),COALESCE(t.output_schema,m.output_schema),
            COALESCE(t.drops,0)
        FROM messages m LEFT JOIN turns t ON t.id=m.id;
    UPDATE turns_whole SET quotes=NULL WHERE quotes='[]';
    UPDATE senders SET quotes=(SELECT json_group_array(json(CASE
            WHEN json_type(q.value,'$.message') IS NULL THEN q.value
            ELSE json_remove(json_set(q.value,'$.turn',json_extract(q.value,'$.message')),'$.message')
            END)) FROM json_each(senders.quotes) q)
        WHERE quotes IS NOT NULL;
    INSERT OR IGNORE INTO retained_turns(turn,bot) SELECT id,bot FROM messages WHERE turn IS NULL OR turn<>id;
    DROP TABLE turns;
    DROP TABLE messages;
    ALTER TABLE turns_whole RENAME TO turns;
    CREATE INDEX turns_bot_id ON turns(bot,id);
    CREATE INDEX turns_started ON turns(bot,id) WHERE started_ms IS NOT NULL;
    CREATE INDEX turns_prompt_node ON turns(prompt_node) WHERE prompt_node IS NOT NULL;
    CREATE INDEX turns_running ON turns(id) WHERE status='running';
    CREATE INDEX turns_waiting ON turns(id) WHERE status='waiting';
    CREATE INDEX turns_paced ON turns(id) WHERE status='paced';
    CREATE INDEX turns_queued ON turns(bot,id) WHERE status='queued';
    CREATE INDEX turns_steers ON turns(bot,id) WHERE status='queued' AND delivery='steer';
    CREATE INDEX turns_attached ON turns(bot,id) WHERE status='attached';
    CREATE INDEX turns_due ON turns(due_ms) WHERE status='attached' AND due_ms IS NOT NULL;
    CREATE INDEX turns_held ON turns(due_ms) WHERE status='ready' AND held IS NOT NULL;
    CREATE INDEX turns_ready ON turns(id) WHERE status='ready';
    CREATE INDEX turns_ready_bot ON turns(bot) WHERE status='ready';
    CREATE TABLE turn_sequence(singleton INTEGER PRIMARY KEY CHECK(singleton=1),
        last_id INTEGER NOT NULL CHECK(last_id>=0));
    INSERT INTO turn_sequence SELECT 1,last_id FROM message_sequence;
    DROP TABLE message_sequence;
    UPDATE events SET turn=message WHERE kind IN ('queued','attached');
    UPDATE events SET turn=message,kind='turn_held' WHERE kind='held';
    INSERT INTO events(bot,turn,kind,data,written_ms)
        SELECT bot,message,'turn_finished',json_object('status','steered','into',turn,
            'node',json_extract(data,'$.node'),'checkpoint',NULL,'error',NULL,'detail',NULL),written_ms
        FROM events WHERE kind='steered' ORDER BY id;
    UPDATE events SET data=json_remove(json_set(data,'$.steer',json_extract(data,'$.message')),'$.message')
        WHERE kind='steered';
    UPDATE events SET turn=message,kind='turn_finished',
            data=json_set(data,'$.status',CASE json_extract(data,'$.status')
                WHEN 'withdrawn' THEN 'interrupted' ELSE json_extract(data,'$.status') END)
        WHERE kind='message_ended';
    DROP INDEX events_message;
    DROP INDEX events_in_line;
    ALTER TABLE events DROP COLUMN message;
    ALTER TABLE nodes DROP COLUMN message;
    PRAGMA user_version=90;";
/// An older store's changes, made to a store first put back in schema 90's
/// shape.
pub trait Older {
    fn older(&self, sql: &str) -> rusqlite::Result<()>;
}
impl Older for Connection {
    fn older(&self, sql: &str) -> rusqlite::Result<()> {
        self.execute_batch(UNSPLIT)?;
        self.execute_batch(sql)
    }
}
