PRAGMA user_version=106;
BEGIN TRANSACTION;
CREATE TABLE answers(id INTEGER PRIMARY KEY AUTOINCREMENT, turn INTEGER NOT NULL REFERENCES turns(id),
                call_id TEXT NOT NULL, tag TEXT NOT NULL, name TEXT NOT NULL, arguments TEXT NOT NULL,
                expires_ms INTEGER NOT NULL, output TEXT, failed INTEGER NOT NULL DEFAULT 0, paused_ms INTEGER,
                parts TEXT, taken_ms INTEGER, process INTEGER, yielded INTEGER NOT NULL DEFAULT 0);
INSERT INTO "answers" VALUES(2,4,'exec-1','sonny','exec','{"input":"await yield_control()"}',1795137343579,NULL,0,NULL,NULL,NULL,NULL,0);
CREATE TABLE approval_tags(approval INTEGER NOT NULL REFERENCES approvals(id) ON DELETE CASCADE,
                tag TEXT NOT NULL, turn INTEGER NOT NULL, PRIMARY KEY(approval,tag)) WITHOUT ROWID;
CREATE TABLE approvals(id INTEGER PRIMARY KEY AUTOINCREMENT, turn INTEGER NOT NULL REFERENCES turns(id),
                call_id TEXT NOT NULL, name TEXT NOT NULL, node INTEGER NOT NULL, request INTEGER NOT NULL,
                announced_ms INTEGER NOT NULL, gates TEXT NOT NULL, verdicts TEXT, arguments TEXT NOT NULL, asked TEXT,
                step_of TEXT, process INTEGER);
CREATE TABLE artifacts(turn INTEGER NOT NULL REFERENCES turns(id), call_id TEXT NOT NULL,
                stream TEXT NOT NULL, data BLOB NOT NULL, raw_bytes INTEGER NOT NULL DEFAULT 0, file TEXT,
                PRIMARY KEY(turn,call_id,stream));
CREATE TABLE audit(id INTEGER PRIMARY KEY AUTOINCREMENT, at_ms INTEGER NOT NULL,
                bot_id INTEGER NOT NULL, bot TEXT NOT NULL, turn INTEGER NOT NULL, call_id TEXT NOT NULL,
                step TEXT, tool TEXT NOT NULL, kind TEXT, place TEXT, approved TEXT NOT NULL,
                access TEXT NOT NULL, ms INTEGER, outcome TEXT NOT NULL, preview TEXT NOT NULL);
INSERT INTO "audit" VALUES(1,1795133743335,1,'Lead',1,'exec-1',NULL,'exec','mcp',NULL,'nobody','unknown',1,'completed','{"input":"text(\"ok\")"}');
INSERT INTO "audit" VALUES(2,1795133743479,3,'Helper',3,'echo-1',NULL,'echo','agents',NULL,'nobody','read',1,'completed','{"text": "hi"}');
INSERT INTO "audit" VALUES(3,1795133743579,1,'Lead',4,'exec-1',NULL,'exec','mcp',NULL,'nobody','unknown',NULL,'unknown','{"input":"await yield_control()"}');
CREATE TABLE bot_sequence(singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                last_id INTEGER NOT NULL CHECK(last_id>=0));
INSERT INTO "bot_sequence" VALUES(1,4);
CREATE TABLE bots(name TEXT PRIMARY KEY, head INTEGER REFERENCES nodes(id),
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
                compaction INTEGER REFERENCES compactions(node),
                compaction_instructions TEXT, compaction_model TEXT,
                cache_bot INTEGER, thinking_prefix INTEGER,
                thinking_floor INTEGER NOT NULL DEFAULT 0,
                fallback TEXT,
                elision INTEGER REFERENCES elisions(node),
                thinking_from INTEGER NOT NULL DEFAULT 0,
                thinking_to INTEGER NOT NULL DEFAULT 0,
                thinking_elided INTEGER NOT NULL DEFAULT 0, gates TEXT, denials TEXT,
                closed INTEGER, open_calls INTEGER, allowed TEXT, settings TEXT, creation TEXT,
                expire_ms INTEGER, expires_at INTEGER,
                sent_bytes INTEGER, sent_tokens INTEGER, compact_asked INTEGER NOT NULL DEFAULT 0,
                declared TEXT, work INTEGER NOT NULL DEFAULT 0, work_read INTEGER,
                budget_counts TEXT, depth INTEGER NOT NULL DEFAULT 0,
                started_ms INTEGER, waiting_on TEXT, last_turn INTEGER, last_status TEXT,
                last_started_ms INTEGER, last_finished_ms INTEGER, last_error TEXT,
                configure_pending TEXT, refused_summary TEXT, closed_ms INTEGER);
INSERT INTO "bots" VALUES('Lead',12,1,'/tmp/agent-store-fixture','waiting',4,'openai','responses','synthetic-model','Test agent.',NULL,NULL,330,1,0,'echo,shell',300,0,NULL,NULL,NULL,NULL,NULL,NULL,NULL,0,NULL,NULL,0,0,0,NULL,NULL,11,1,NULL,'{"approval_hold_ms":0,"steps_only":["*"]}',NULL,NULL,NULL,1255,100,0,'2da115731fe44fc54c25abfde7af8894e59eecc2ae079e1fdcf4c83445b1befb',145,12,NULL,0,1795133743530,'{"answer":"sonny","call_id":"exec-1","deadline_ms":1795137343579}',1,'completed',1795133743329,1795133743383,NULL,NULL,NULL,NULL);
INSERT INTO "bots" VALUES('Lead.task',6,2,'/tmp/agent-store-fixture','completed',NULL,'openai','responses','synthetic-model','Test agent.',NULL,NULL,110,5,0,'echo,shell',100,0,NULL,NULL,NULL,NULL,NULL,NULL,NULL,0,NULL,NULL,0,0,0,NULL,NULL,NULL,NULL,NULL,'{"steps_only":["*"]}',NULL,NULL,NULL,382,100,0,'78f94e332548632cd23eee9ddc2660df79e87cf27fdbc36ff6048cb80a8b2256',110,6,NULL,0,NULL,NULL,2,'completed',1795133743385,1795133743431,NULL,NULL,NULL,NULL);
INSERT INTO "bots" VALUES('Helper',10,3,'/tmp/agent-store-fixture','completed',NULL,'openai','responses','synthetic-model','Test agent.',NULL,NULL,220,7,0,'echo,shell',200,0,NULL,NULL,NULL,NULL,NULL,NULL,NULL,0,NULL,NULL,0,0,0,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,3112,100,0,NULL,122,10,NULL,0,NULL,NULL,3,'completed',1795133743432,1795133743527,NULL,NULL,NULL,NULL);
INSERT INTO "bots" VALUES('Helper.fork',10,4,'/tmp/agent-store-fixture','idle',NULL,'openai','responses','synthetic-model','Test agent.',NULL,NULL,0,7,0,'echo,shell',0,0,NULL,NULL,NULL,NULL,NULL,3,NULL,0,NULL,NULL,0,0,0,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,0,NULL,0,10,NULL,0,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL);
CREATE TABLE checkpoints(bot TEXT NOT NULL REFERENCES bots(name), head INTEGER NOT NULL REFERENCES nodes(id),
                PRIMARY KEY(bot,head));
INSERT INTO "checkpoints" VALUES('Lead',4);
INSERT INTO "checkpoints" VALUES('Lead.task',6);
INSERT INTO "checkpoints" VALUES('Helper',10);
INSERT INTO "checkpoints" VALUES('Helper.fork',10);
CREATE TABLE compactions(node INTEGER PRIMARY KEY REFERENCES nodes(id),
                previous INTEGER REFERENCES compactions(node), cut INTEGER NOT NULL REFERENCES nodes(id),
                summary TEXT NOT NULL, prompts TEXT NOT NULL,
                covered_from INTEGER NOT NULL, covered_to INTEGER NOT NULL,
                pinned INTEGER REFERENCES nodes(id));
CREATE TABLE companions(name TEXT PRIMARY KEY, spec TEXT NOT NULL);
CREATE TABLE declined(node INTEGER PRIMARY KEY REFERENCES nodes(id),
                item BLOB NOT NULL);
CREATE TABLE deleted_bots(id INTEGER PRIMARY KEY, name TEXT NOT NULL,
                request_id TEXT);
CREATE TABLE elisions(node INTEGER PRIMARY KEY REFERENCES nodes(id),
                previous INTEGER REFERENCES elisions(node), through INTEGER NOT NULL,
                saved INTEGER NOT NULL);
CREATE TABLE event_retention(singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                pruned_cursor INTEGER NOT NULL);
INSERT INTO "event_retention" VALUES(1,0);
CREATE TABLE events(id INTEGER PRIMARY KEY AUTOINCREMENT, bot TEXT NOT NULL REFERENCES bots(name),
                turn INTEGER, kind TEXT NOT NULL, data TEXT NOT NULL, written_ms INTEGER, message INTEGER);
INSERT INTO "events" VALUES(1,'Lead',NULL,'created','{"bot_id":1,"created_by":null,"created_by_id":null,"depth":0,"effort":null,"last":null,"model":"synthetic-model","provider":"openai","running_turn":null,"started_ms":null,"status":"idle","waiting_on":null,"workspace":"/tmp/agent-store-fixture"}',1795133743324,NULL);
INSERT INTO "events" VALUES(2,'Lead.task',NULL,'created','{"bot_id":2,"created_by":null,"created_by_id":null,"depth":0,"effort":null,"last":null,"model":"synthetic-model","provider":"openai","running_turn":null,"started_ms":null,"status":"idle","waiting_on":null,"workspace":"/tmp/agent-store-fixture"}',1795133743325,NULL);
INSERT INTO "events" VALUES(3,'Helper',NULL,'created','{"bot_id":3,"created_by":null,"created_by_id":null,"depth":0,"effort":null,"last":null,"model":"synthetic-model","provider":"openai","running_turn":null,"started_ms":null,"status":"idle","waiting_on":null,"workspace":"/tmp/agent-store-fixture"}',1795133743327,NULL);
INSERT INTO "events" VALUES(4,'Lead',1,'accepted','{"effort":null,"model":"openai/synthetic-model","node":1,"request_id":"script","workspace":"/tmp/agent-store-fixture"}',1795133743329,1);
INSERT INTO "events" VALUES(5,'Lead',1,'message','{"node":2}',1795133743334,NULL);
INSERT INTO "events" VALUES(6,'Lead',1,'usage','{"cached_input_tokens":0,"context_window":200000,"daemon_ms":1,"first_token_ms":1,"input_tokens":100,"model_ms":1,"output_tokens":10,"sent_ms":1795133743332,"served_model":"synthetic-model-2026-09-26","speed":"standard","work_tokens":110}',1795133743334,NULL);
INSERT INTO "events" VALUES(7,'Lead',1,'tool_started','{"answered_by":"sonny","arguments":"{\"input\":\"text(\\\"ok\\\")\"}","arguments_truncated":false,"call_id":"exec-1","expires_ms":1795137343335,"name":"exec"}',1795133743335,NULL);
INSERT INTO "events" VALUES(8,'Lead',1,'tool_completed','{"artifacts":[],"call_id":"exec-1","node":3}',1795133743336,NULL);
INSERT INTO "events" VALUES(9,'Lead',1,'message','{"node":4}',1795133743382,NULL);
INSERT INTO "events" VALUES(10,'Lead',1,'usage','{"cached_input_tokens":0,"context_window":200000,"daemon_ms":1,"first_token_ms":44,"input_tokens":100,"model_ms":44,"output_tokens":10,"sent_ms":1795133743337,"served_model":"synthetic-model-2026-09-26","speed":"standard","work_tokens":16}',1795133743382,NULL);
INSERT INTO "events" VALUES(11,'Lead',1,'turn_finished','{"checkpoint":4,"detail":null,"error":null,"status":"completed"}',1795133743383,NULL);
INSERT INTO "events" VALUES(12,'Lead.task',2,'accepted','{"effort":null,"model":"openai/synthetic-model","node":5,"request_id":"hello","workspace":"/tmp/agent-store-fixture"}',1795133743385,2);
INSERT INTO "events" VALUES(13,'Lead.task',2,'message','{"node":6}',1795133743429,NULL);
INSERT INTO "events" VALUES(14,'Lead.task',2,'usage','{"cached_input_tokens":0,"context_window":200000,"first_token_ms":41,"input_tokens":100,"model_ms":41,"output_tokens":10,"sent_ms":1795133743388,"served_model":"synthetic-model-2026-09-26","speed":"standard","work_tokens":110}',1795133743429,NULL);
INSERT INTO "events" VALUES(15,'Lead.task',2,'turn_finished','{"checkpoint":6,"detail":null,"error":null,"status":"completed"}',1795133743431,NULL);
INSERT INTO "events" VALUES(16,'Helper',3,'accepted','{"effort":null,"model":"openai/synthetic-model","node":7,"request_id":"echo","workspace":"/tmp/agent-store-fixture"}',1795133743432,3);
INSERT INTO "events" VALUES(17,'Helper',3,'message','{"node":8}',1795133743477,NULL);
INSERT INTO "events" VALUES(18,'Helper',3,'usage','{"cached_input_tokens":0,"context_window":200000,"first_token_ms":42,"input_tokens":100,"model_ms":42,"output_tokens":10,"sent_ms":1795133743435,"served_model":"synthetic-model-2026-09-26","speed":"standard","work_tokens":110}',1795133743477,NULL);
INSERT INTO "events" VALUES(19,'Helper',3,'tool_started','{"arguments":"{\"text\": \"hi\"}","arguments_truncated":false,"call_id":"echo-1","name":"echo"}',1795133743479,NULL);
INSERT INTO "events" VALUES(20,'Helper',3,'tool_completed','{"artifacts":[],"call_id":"echo-1","node":9}',1795133743480,NULL);
INSERT INTO "events" VALUES(21,'Helper',3,'message','{"node":10}',1795133743525,NULL);
INSERT INTO "events" VALUES(22,'Helper',3,'usage','{"cached_input_tokens":0,"context_window":200000,"daemon_ms":1,"first_token_ms":44,"input_tokens":100,"model_ms":44,"output_tokens":10,"sent_ms":1795133743481,"served_model":"synthetic-model-2026-09-26","speed":"standard","work_tokens":12}',1795133743526,NULL);
INSERT INTO "events" VALUES(23,'Helper',3,'turn_finished','{"checkpoint":10,"detail":null,"error":null,"status":"completed"}',1795133743527,NULL);
INSERT INTO "events" VALUES(24,'Helper.fork',NULL,'forked','{"bot_id":4,"checkpoint":10,"created_by":null,"created_by_id":null,"depth":0,"effort":null,"last":null,"model":"synthetic-model","node":10,"provider":"openai","running_turn":null,"source":"Helper","started_ms":null,"status":"idle","waiting_on":null,"workspace":"/tmp/agent-store-fixture"}',1795133743528,NULL);
INSERT INTO "events" VALUES(25,'Lead',4,'accepted','{"effort":null,"model":"openai/synthetic-model","node":11,"request_id":"park","workspace":"/tmp/agent-store-fixture"}',1795133743530,4);
INSERT INTO "events" VALUES(26,'Lead',4,'message','{"node":12}',1795133743577,NULL);
INSERT INTO "events" VALUES(27,'Lead',4,'usage','{"cached_input_tokens":0,"context_window":200000,"first_token_ms":44,"input_tokens":100,"model_ms":44,"output_tokens":10,"sent_ms":1795133743533,"served_model":"synthetic-model-2026-09-26","speed":"standard","work_tokens":19}',1795133743577,NULL);
INSERT INTO "events" VALUES(28,'Lead',4,'tool_started','{"answered_by":"sonny","arguments":"{\"input\":\"await yield_control()\"}","arguments_truncated":false,"call_id":"exec-1","expires_ms":1795137343579,"name":"exec"}',1795133743579,NULL);
INSERT INTO "events" VALUES(29,'Lead',4,'turn_waiting','{"answer":"sonny","call_id":"exec-1","deadline_ms":1795137343579}',1795133743831,NULL);
INSERT INTO "events" VALUES(30,'Lead',NULL,'queued','{"delivery":"queue","effort":null,"model":"openai/synthetic-model","request_id":"queued","status":"queued","workspace":"/tmp/agent-store-fixture"}',1795133743833,5);
CREATE TABLE lineage(node INTEGER PRIMARY KEY REFERENCES nodes(id),
                parent INTEGER, depth INTEGER NOT NULL, skip INTEGER, start INTEGER);
INSERT INTO "lineage" VALUES(1,NULL,1,NULL,1);
INSERT INTO "lineage" VALUES(2,1,2,1,1);
INSERT INTO "lineage" VALUES(3,2,3,1,1);
INSERT INTO "lineage" VALUES(4,3,4,2,1);
INSERT INTO "lineage" VALUES(5,NULL,1,NULL,5);
INSERT INTO "lineage" VALUES(6,5,2,5,5);
INSERT INTO "lineage" VALUES(7,NULL,1,NULL,7);
INSERT INTO "lineage" VALUES(8,7,2,7,7);
INSERT INTO "lineage" VALUES(9,8,3,7,7);
INSERT INTO "lineage" VALUES(10,9,4,8,7);
INSERT INTO "lineage" VALUES(11,4,5,1,11);
INSERT INTO "lineage" VALUES(12,11,6,2,11);
CREATE TABLE message_sequence(singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                last_id INTEGER NOT NULL CHECK(last_id>=0));
INSERT INTO "message_sequence" VALUES(1,5);
CREATE TABLE messages(id INTEGER PRIMARY KEY, bot TEXT NOT NULL REFERENCES bots(name),
                request_id TEXT NOT NULL, delivery TEXT NOT NULL, status TEXT NOT NULL,
                turn INTEGER, node INTEGER REFERENCES nodes(id), text TEXT NOT NULL,
                images TEXT, quotes TEXT, from_bot TEXT, from_turn INTEGER, from_id INTEGER,
                origin TEXT, person TEXT, expected_turn INTEGER,
                workspace TEXT, model TEXT, reasoning TEXT, budget_tokens INTEGER, budget_counts TEXT,
                output_schema TEXT, wait_ms INTEGER, due_ms INTEGER, held TEXT,
                sent_ms INTEGER, taken_ms INTEGER,
                UNIQUE(bot,request_id));
INSERT INTO "messages" VALUES(1,'Lead','script','reject','taken',1,1,'run a script',NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,'/tmp/agent-store-fixture',NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,1795133743328,1795133743328);
INSERT INTO "messages" VALUES(2,'Lead.task','hello','reject','taken',2,5,'hello',NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,'/tmp/agent-store-fixture',NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,1795133743385,1795133743385);
INSERT INTO "messages" VALUES(3,'Helper','echo','reject','taken',3,7,'calls:[["echo", {"text": "hi"}]]',NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,'/tmp/agent-store-fixture',NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,1795133743432,1795133743432);
INSERT INTO "messages" VALUES(4,'Lead','park','reject','taken',4,11,'wait on a script',NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,'/tmp/agent-store-fixture',NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,1795133743530,1795133743530);
INSERT INTO "messages" VALUES(5,'Lead','queued','queue','queued',NULL,NULL,'after the script',NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,'/tmp/agent-store-fixture',NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,1795133743833,NULL);
CREATE TABLE node_sequence(singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                last_id INTEGER NOT NULL CHECK(last_id>=0));
INSERT INTO "node_sequence" VALUES(1,0);
CREATE TABLE nodes(id INTEGER PRIMARY KEY, parent INTEGER REFERENCES nodes(id),
                item BLOB NOT NULL, total_bytes INTEGER NOT NULL, depth INTEGER NOT NULL,
                turn INTEGER, turn_seq INTEGER, thinking INTEGER NOT NULL DEFAULT 0,
                elided INTEGER NOT NULL DEFAULT 0, total_elided INTEGER NOT NULL DEFAULT 0,
                total_thinking INTEGER NOT NULL DEFAULT 0, pictured INTEGER NOT NULL DEFAULT 0,
                pictures INTEGER NOT NULL DEFAULT 0, written_ms INTEGER, message INTEGER);
INSERT INTO "nodes" VALUES(1,NULL,X'7B22636F6E74656E74223A5B7B2274657874223A2272756E206120736372697074222C2274797065223A22696E7075745F74657874227D2C7B2274657874223A225B4D657373616765204C6561642F312E5D222C2274797065223A22696E7075745F74657874227D5D2C22726F6C65223A2275736572227D',120,1,1,1,0,0,0,0,0,0,1795133743329,1);
INSERT INTO "nodes" VALUES(2,1,X'7B2274797065223A2022637573746F6D5F746F6F6C5F63616C6C222C20226E616D65223A202265786563222C202263616C6C5F6964223A2022657865632D31222C2022696E707574223A202274657874285C226F6B5C2229227D',210,2,NULL,NULL,0,0,0,0,0,0,1795133743334,NULL);
INSERT INTO "nodes" VALUES(3,2,X'7B2263616C6C5F6964223A22657865632D31222C226F7574707574223A226F6B222C2274797065223A22637573746F6D5F746F6F6C5F63616C6C5F6F7574707574227D',277,3,NULL,NULL,0,0,0,0,0,0,1795133743336,NULL);
INSERT INTO "nodes" VALUES(4,3,X'7B2274797065223A20226D657373616765222C2022726F6C65223A2022617373697374616E74222C2022636F6E74656E74223A205B7B2274797065223A20226F75747075745F74657874222C202274657874223A2022746865207363726970742073616964206F6B227D5D7D',385,4,NULL,NULL,0,0,0,0,0,0,1795133743381,NULL);
INSERT INTO "nodes" VALUES(5,NULL,X'7B22636F6E74656E74223A5B7B2274657874223A2268656C6C6F222C2274797065223A22696E7075745F74657874227D2C7B2274657874223A225B4D657373616765204C6561642E7461736B2F322E5D222C2274797065223A22696E7075745F74657874227D5D2C22726F6C65223A2275736572227D',118,1,2,1,0,0,0,0,0,0,1795133743385,2);
INSERT INTO "nodes" VALUES(6,5,X'7B2274797065223A20226D657373616765222C2022726F6C65223A2022617373697374616E74222C2022636F6E74656E74223A205B7B2274797065223A20226F75747075745F74657874222C202274657874223A202268656172643A2068656C6C6F227D5D7D',220,2,NULL,NULL,0,0,0,0,0,0,1795133743429,NULL);
INSERT INTO "nodes" VALUES(7,NULL,X'7B22636F6E74656E74223A5B7B2274657874223A2263616C6C733A5B5B5C226563686F5C222C207B5C22746578745C223A205C2268695C227D5D5D222C2274797065223A22696E7075745F74657874227D2C7B2274657874223A225B4D6573736167652048656C7065722F332E5D222C2274797065223A22696E7075745F74657874227D5D2C22726F6C65223A2275736572227D',148,1,3,1,0,0,0,0,0,0,1795133743432,3);
INSERT INTO "nodes" VALUES(8,7,X'7B2274797065223A202266756E6374696F6E5F63616C6C222C20226E616D65223A20226563686F222C202263616C6C5F6964223A20226563686F2D31222C2022617267756D656E7473223A20227B5C22746578745C223A205C2268695C227D227D',245,2,NULL,NULL,0,0,0,0,0,0,1795133743477,NULL);
INSERT INTO "nodes" VALUES(9,8,X'7B2263616C6C5F6964223A226563686F2D31222C226F7574707574223A226869222C2274797065223A2266756E6374696F6E5F63616C6C5F6F7574707574227D',309,3,NULL,NULL,0,0,0,0,0,0,1795133743480,NULL);
INSERT INTO "nodes" VALUES(10,9,X'7B226964223A20226D73675F6563686F222C202274797065223A20226D657373616765222C2022726F6C65223A2022617373697374616E74222C2022636F6E74656E74223A205B7B2274797065223A20226F75747075745F74657874222C202274657874223A20226563686F3A6869227D5D7D',424,4,NULL,NULL,0,0,0,0,0,0,1795133743525,NULL);
INSERT INTO "nodes" VALUES(11,4,X'7B22636F6E74656E74223A5B7B2274657874223A2277616974206F6E206120736372697074222C2274797065223A22696E7075745F74657874227D2C7B2274657874223A225B4D657373616765204C6561642F342E5D222C2274797065223A22696E7075745F74657874227D5D2C22726F6C65223A2275736572227D',509,5,4,2,0,0,0,0,0,0,1795133743530,4);
INSERT INTO "nodes" VALUES(12,11,X'7B2274797065223A2022637573746F6D5F746F6F6C5F63616C6C222C20226E616D65223A202265786563222C202263616C6C5F6964223A2022657865632D31222C2022696E707574223A20226177616974207969656C645F636F6E74726F6C2829227D',608,6,NULL,NULL,0,0,0,0,0,0,1795133743577,NULL);
CREATE TABLE openings(node INTEGER PRIMARY KEY REFERENCES nodes(id),
                text TEXT, chars INTEGER);
INSERT INTO "openings" VALUES(1,'run a script',12);
INSERT INTO "openings" VALUES(5,'hello',5);
INSERT INTO "openings" VALUES(7,'calls:[["echo", {"text": "hi"}]]',32);
INSERT INTO "openings" VALUES(11,'wait on a script',16);
CREATE TABLE pauses(id INTEGER PRIMARY KEY AUTOINCREMENT,
                reason TEXT NOT NULL, pulled_by TEXT, at_ms INTEGER NOT NULL, parked TEXT,
                resumed_by TEXT, resumed_ms INTEGER);
CREATE TABLE pins(compaction INTEGER NOT NULL REFERENCES compactions(node),
                node INTEGER NOT NULL REFERENCES nodes(id), PRIMARY KEY(compaction,node)) WITHOUT ROWID;
CREATE TABLE processes(id INTEGER PRIMARY KEY AUTOINCREMENT, turn INTEGER NOT NULL REFERENCES turns(id),
                call_id TEXT NOT NULL, status TEXT NOT NULL, result TEXT, delivered INTEGER, told INTEGER, bot TEXT,
                call TEXT, kind TEXT, command TEXT, started_ms INTEGER, detached TEXT);
CREATE TABLE retained_turns(turn INTEGER PRIMARY KEY REFERENCES turns(id) ON DELETE CASCADE,
                bot TEXT NOT NULL REFERENCES bots(name));
INSERT INTO "retained_turns" VALUES(1,'Lead');
INSERT INTO "retained_turns" VALUES(2,'Lead.task');
INSERT INTO "retained_turns" VALUES(3,'Helper');
INSERT INTO "retained_turns" VALUES(4,'Lead');
CREATE TABLE senders(node INTEGER PRIMARY KEY REFERENCES nodes(id),
                bot TEXT, bot_id INTEGER, turn INTEGER, origin TEXT, person TEXT, quotes TEXT,
                attached INTEGER);
CREATE TABLE steps(turn INTEGER NOT NULL, call_id TEXT NOT NULL,
                step_of TEXT NOT NULL, bot TEXT NOT NULL, audit INTEGER, started_ms INTEGER NOT NULL,
                ended_ms INTEGER, PRIMARY KEY(turn,call_id)) WITHOUT ROWID;
CREATE TABLE store(singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                identity INTEGER NOT NULL);
INSERT INTO "store" VALUES(1,-6136855000854524370);
CREATE TABLE stubs(node INTEGER PRIMARY KEY REFERENCES nodes(id),
                item BLOB NOT NULL);
CREATE TABLE tool_lists(digest TEXT PRIMARY KEY, tools TEXT NOT NULL);
INSERT INTO "tool_lists" VALUES('2da115731fe44fc54c25abfde7af8894e59eecc2ae079e1fdcf4c83445b1befb','[{"name":"exec","description":"Run JavaScript that calls tools.","parameters":{"additionalProperties":false,"properties":{"input":{"description":"Raw JavaScript.","type":"string"}},"required":["input"],"type":"object"},"grammar":"start: SOURCE\nSOURCE: /(.|\\n)+/","result":"ok","answered_by":"sonny","expire_ms":3600000,"inherit":1,"steps":true,"gate_daemon_steps":true,"lift":true,"kind":"mcp"},{"name":"browser_open","description":"Open a page.","parameters":{"properties":{"url":{"type":"string"}},"required":["url"],"type":"object"},"result":"ok","answered_by":"sonny","kind":"browser"},{"name":"desktop_click","description":"Click a point.","parameters":{"properties":{"x":{"minimum":1e-7,"type":"number"},"y":{"type":"number"}},"type":"object"},"result":"ok","answered_by":"sonny","kind":"desktop"},{"name":"note","description":"Keep a note.","parameters":{"type":"object"},"result":"Noted.","inherit":false,"kind":"file"}]');
INSERT INTO "tool_lists" VALUES('78f94e332548632cd23eee9ddc2660df79e87cf27fdbc36ff6048cb80a8b2256','[{"name":"exec","description":"Run JavaScript that calls tools.","parameters":{"additionalProperties":false,"properties":{"input":{"description":"Raw JavaScript.","type":"string"}},"required":["input"],"type":"object"},"grammar":"start: SOURCE\nSOURCE: /(.|\\n)+/","result":"ok","answered_by":"sonny","expire_ms":3600000,"inherit":1,"steps":true,"gate_daemon_steps":true,"lift":true,"kind":"mcp"}]');
CREATE TABLE tools(turn INTEGER NOT NULL REFERENCES turns(id), call_id TEXT NOT NULL,
                status TEXT NOT NULL, custom INTEGER NOT NULL DEFAULT 0, ruling TEXT, audit INTEGER,
                PRIMARY KEY(turn,call_id));
INSERT INTO "tools" VALUES(1,'exec-1','completed',1,NULL,1);
INSERT INTO "tools" VALUES(3,'echo-1','completed',0,NULL,2);
INSERT INTO "tools" VALUES(4,'exec-1','executing',1,NULL,3);
CREATE TABLE turn_models(turn INTEGER NOT NULL REFERENCES turns(id) ON DELETE CASCADE,
                model TEXT NOT NULL, speed TEXT NOT NULL DEFAULT 'standard', input_tokens INTEGER NOT NULL DEFAULT 0,
                cached_input_tokens INTEGER NOT NULL DEFAULT 0, cache_write_tokens INTEGER NOT NULL DEFAULT 0,
                cache_write_1h_tokens INTEGER NOT NULL DEFAULT 0, output_tokens INTEGER NOT NULL DEFAULT 0,
                timed_rounds INTEGER NOT NULL DEFAULT 0, timed_output_tokens INTEGER NOT NULL DEFAULT 0,
                model_ms INTEGER NOT NULL DEFAULT 0, first_token_ms INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY(turn,model,speed)) WITHOUT ROWID;
INSERT INTO "turn_models" VALUES(1,'openai/synthetic-model','standard',200,0,0,0,20,2,20,45,45);
INSERT INTO "turn_models" VALUES(2,'openai/synthetic-model','standard',100,0,0,0,10,1,10,41,41);
INSERT INTO "turn_models" VALUES(3,'openai/synthetic-model','standard',200,0,0,0,20,2,20,86,86);
INSERT INTO "turn_models" VALUES(4,'openai/synthetic-model','standard',100,0,0,0,10,1,10,44,44);
CREATE TABLE turns(id INTEGER PRIMARY KEY REFERENCES messages(id),
                bot TEXT NOT NULL REFERENCES bots(name), status TEXT NOT NULL,
                workspace TEXT, model TEXT, reasoning TEXT, bot_model TEXT, bot_reasoning TEXT,
                budget_tokens INTEGER, budget_counts TEXT, output_schema TEXT,
                waiting TEXT, model_rounds INTEGER NOT NULL DEFAULT 0, view_sent INTEGER,
                input_tokens INTEGER NOT NULL DEFAULT 0, output_tokens INTEGER NOT NULL DEFAULT 0,
                cached_input_tokens INTEGER NOT NULL DEFAULT 0,
                started_ms INTEGER NOT NULL, finished_ms INTEGER,
                retries INTEGER NOT NULL DEFAULT 0, paced_ms INTEGER NOT NULL DEFAULT 0,
                summary_ms INTEGER NOT NULL DEFAULT 0, reply_node INTEGER,
                work INTEGER NOT NULL DEFAULT 0, limited TEXT, drops INTEGER NOT NULL DEFAULT 0,
                error TEXT, daemon_ms INTEGER NOT NULL DEFAULT 0,
                tool_calls INTEGER NOT NULL DEFAULT 0, tool_ms INTEGER NOT NULL DEFAULT 0,
                tools_until_ms INTEGER, timed_rounds INTEGER NOT NULL DEFAULT 0,
                timed_output_tokens INTEGER NOT NULL DEFAULT 0, model_ms INTEGER NOT NULL DEFAULT 0,
                first_token_ms INTEGER NOT NULL DEFAULT 0);
INSERT INTO "turns" VALUES(1,'Lead','completed','/tmp/agent-store-fixture',NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,2,1,200,20,0,1795133743329,1795133743383,0,0,0,4,126,NULL,0,NULL,2,1,1,1795133743336,2,20,45,45);
INSERT INTO "turns" VALUES(2,'Lead.task','completed','/tmp/agent-store-fixture',NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,1,1,100,10,0,1795133743385,1795133743431,0,0,0,6,110,NULL,0,NULL,0,0,0,NULL,1,10,41,41);
INSERT INTO "turns" VALUES(3,'Helper','completed','/tmp/agent-store-fixture',NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,2,1,200,20,0,1795133743432,1795133743527,0,0,0,10,122,NULL,0,NULL,1,1,1,1795133743480,2,20,86,86);
INSERT INTO "turns" VALUES(4,'Lead','waiting','/tmp/agent-store-fixture',NULL,NULL,NULL,NULL,NULL,NULL,NULL,'{"turn":4,"bot":"Lead","call_id":"exec-1","handles":[],"deadline_ms":1795137343579,"any":false,"pending":[],"paced_since_ms":null,"call_attempts":0,"call_spent_ms":0,"compaction":false,"answer":"sonny"}',1,1,100,10,0,1795133743530,NULL,0,0,0,NULL,19,NULL,0,NULL,0,0,0,NULL,1,10,44,44);
CREATE INDEX nodes_turn_seq ON nodes(turn_seq) WHERE turn_seq IS NOT NULL;
CREATE UNIQUE INDEX nodes_turn ON nodes(turn) WHERE turn IS NOT NULL;
CREATE INDEX compactions_previous ON compactions(previous);
CREATE INDEX compactions_cut ON compactions(cut);
CREATE INDEX compactions_pinned ON compactions(pinned) WHERE pinned IS NOT NULL;
CREATE INDEX pins_node ON pins(node);
CREATE INDEX elisions_previous ON elisions(previous);
CREATE UNIQUE INDEX bots_id ON bots(id);
CREATE INDEX bots_compaction ON bots(compaction);
CREATE INDEX bots_elision ON bots(elision);
CREATE INDEX bots_configure_pending ON bots(name)
                WHERE configure_pending IS NOT NULL;
CREATE INDEX bots_declared ON bots(declared) WHERE declared IS NOT NULL;
CREATE INDEX steps_running ON steps(turn,call_id) WHERE ended_ms IS NULL;
CREATE INDEX deleted_bots_key ON deleted_bots(name,request_id)
                WHERE request_id IS NOT NULL;
CREATE INDEX nodes_parent ON nodes(parent);
CREATE INDEX bots_head ON bots(head);
CREATE INDEX bots_context_start ON bots(context_start);
CREATE INDEX messages_bot_id ON messages(bot,id);
CREATE INDEX messages_turn ON messages(turn) WHERE turn IS NOT NULL;
CREATE INDEX messages_node ON messages(node) WHERE node IS NOT NULL;
CREATE INDEX messages_queued ON messages(bot,id) WHERE status='queued';
CREATE INDEX messages_steers ON messages(bot,id) WHERE status='queued' AND delivery='steer';
CREATE INDEX messages_attached ON messages(bot,id) WHERE status='attached';
CREATE INDEX messages_persons ON messages(bot,id)
                WHERE status='taken' AND from_bot IS NULL AND origin IS NULL AND person IS NULL;
CREATE INDEX messages_due ON messages(due_ms) WHERE status='attached' AND due_ms IS NOT NULL;
CREATE INDEX messages_held ON messages(due_ms) WHERE status='ready' AND held IS NOT NULL;
CREATE INDEX messages_ready ON messages(id) WHERE status='ready';
CREATE INDEX messages_ready_bot ON messages(bot) WHERE status='ready';
CREATE INDEX turns_bot_id ON turns(bot,id);
CREATE INDEX turns_started_ms ON turns(started_ms);
CREATE INDEX retained_turns_bot ON retained_turns(bot,turn);
CREATE UNIQUE INDEX pauses_held ON pauses(ifnull(resumed_ms,0))
                WHERE resumed_ms IS NULL;
CREATE UNIQUE INDEX approvals_call ON approvals(turn,call_id);
CREATE INDEX approvals_steps ON approvals(turn,step_of) WHERE step_of IS NOT NULL;
CREATE INDEX approval_tags_tag ON approval_tags(tag,approval,turn);
CREATE UNIQUE INDEX answers_call ON answers(turn,call_id);
CREATE INDEX answers_waiting ON answers(tag,id) WHERE output IS NULL;
CREATE INDEX answers_paused ON answers(turn) WHERE paused_ms IS NOT NULL;
CREATE INDEX processes_turn ON processes(turn);
CREATE INDEX processes_untold ON processes(bot,id)
                WHERE told IS NULL AND delivered IS NULL AND status<>'running';
CREATE INDEX events_bot_cursor ON events(bot,id);
CREATE INDEX events_turn_kind_cursor ON events(turn,kind,id);
CREATE INDEX events_kind_cursor ON events(kind,id);
CREATE INDEX events_message ON events(message) WHERE message IS NOT NULL;
CREATE INDEX events_in_line ON events(bot,message)
                WHERE turn IS NULL AND message IS NOT NULL;
CREATE INDEX checkpoints_head ON checkpoints(head);
CREATE INDEX turns_running ON turns(id) WHERE status='running';
CREATE INDEX turns_waiting ON turns(id) WHERE status='waiting';
CREATE INDEX turns_paced ON turns(id) WHERE status='paced';
CREATE INDEX processes_running ON processes(turn) WHERE status='running';
CREATE INDEX approvals_lifted ON approvals(process) WHERE process IS NOT NULL;
CREATE INDEX processes_bot_running ON processes(bot,id) WHERE status='running';
CREATE INDEX answers_lifted ON answers(process) WHERE process IS NOT NULL;
CREATE INDEX bots_deleting ON bots(name) WHERE status='deleting';
CREATE INDEX bots_running ON bots(name) WHERE running_turn IS NOT NULL;
CREATE INDEX bots_maker ON bots(created_by_id, name) WHERE created_by_id IS NOT NULL;
CREATE INDEX bots_expiring ON bots(expires_at) WHERE expires_at IS NOT NULL;
DELETE FROM "sqlite_sequence";
INSERT INTO "sqlite_sequence" VALUES('events',30);
INSERT INTO "sqlite_sequence" VALUES('audit',3);
INSERT INTO "sqlite_sequence" VALUES('answers',2);
COMMIT;
