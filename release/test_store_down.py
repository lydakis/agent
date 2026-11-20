import contextlib
import fcntl
import glob
import io
import json
import os
import sqlite3
import tempfile
import unittest
from pathlib import Path

import store_down

FIXTURES = Path(__file__).resolve().parent.parent / "tests" / "fixtures"

UP_111 = """
DROP INDEX nodes_turn;
CREATE INDEX nodes_turn ON nodes(turn) WHERE turn IS NOT NULL;
ALTER TABLE bots ADD COLUMN family_start INTEGER;
CREATE INDEX bots_family_start ON bots(family_start) WHERE family_start IS NOT NULL;
CREATE TABLE switches(id INTEGER PRIMARY KEY AUTOINCREMENT,
    bot_id INTEGER, node INTEGER, root INTEGER, first INTEGER, left INTEGER,
    back INTEGER NOT NULL, from_provider TEXT NOT NULL, from_family TEXT NOT NULL,
    from_model TEXT NOT NULL, from_reasoning TEXT, to_provider TEXT NOT NULL,
    to_family TEXT NOT NULL, to_model TEXT NOT NULL, to_reasoning TEXT,
    compaction INTEGER, state TEXT NOT NULL, at_ms INTEGER NOT NULL);
CREATE INDEX switches_bot ON switches(bot_id,id);
CREATE INDEX switches_left ON switches(left) WHERE left IS NOT NULL;
CREATE TABLE translated(node INTEGER PRIMARY KEY, source INTEGER NOT NULL);
CREATE INDEX translated_source ON translated(source);
CREATE TABLE worded(node INTEGER PRIMARY KEY, item BLOB NOT NULL);
CREATE TABLE translating(id INTEGER PRIMARY KEY, bot_id INTEGER UNIQUE, tip INTEGER);
PRAGMA user_version=111;
"""


class StoreDownTest(unittest.TestCase):
    """The way back on the store 0.10.2 made; tests/test_release_stores.py
    opens what it leaves with each release's own daemon."""

    def setUp(self):
        self.folder = tempfile.mkdtemp()
        self.store = os.path.join(self.folder, "state.sqlite")
        conn = sqlite3.connect(self.store)
        conn.executescript((FIXTURES / "store-v0.10.2.sql").read_text())
        # 109 differs from 108 only in that no list says `lift`, as none
        # 0.10.2 wrote does.
        # 110 adds each bot's profile.
        conn.execute("ALTER TABLE bots ADD COLUMN profile TEXT")
        conn.execute("UPDATE bots SET profile='reviewer' WHERE name='Helper'")
        conn.execute("PRAGMA user_version=110")
        # 111 as the daemon makes it (src/store/db.rs), with Helper switched
        # to the other family: its newest prompt written again on a branch
        # from the root, holding the same turn, and Helper moved onto it.
        conn.executescript(UP_111)
        node, turn = conn.execute(
            "SELECT n.id,n.turn FROM nodes n JOIN turns t ON t.id=n.turn"
            " WHERE t.bot='Helper' ORDER BY n.id DESC LIMIT 1"
        ).fetchone()
        conn.execute("CREATE TEMP TABLE copy AS SELECT * FROM nodes WHERE id=?", (node,))
        conn.execute("UPDATE copy SET id=(SELECT max(id)+1 FROM nodes),parent=NULL,depth=0")
        conn.execute("INSERT INTO nodes SELECT * FROM copy")
        self.switched = conn.execute("SELECT max(id) FROM nodes").fetchone()[0]
        self.original, self.turn = node, turn
        conn.execute("UPDATE bots SET head=?1,family_start=?1 WHERE name='Helper'", (self.switched,))
        conn.execute(
            "INSERT INTO switches(bot_id,node,left,back,from_provider,from_family,from_model,"
            "to_provider,to_family,to_model,state,at_ms) SELECT id,?,?,0,'openai','responses','a',"
            "'anthropic','anthropic','b','{}',0 FROM bots WHERE name='Helper'",
            (self.switched, node),
        )
        conn.execute(
            "UPDATE bots SET settings=json_set(coalesce(settings,'{}'),'$.namespace','fixture')"
            " WHERE name='Helper'"
        )
        conn.execute(
            "UPDATE bots SET creation=json_object('request',json_object('op','create','settings',"
            "json_object('namespace','fixture','speeds',json_object('openai/*',json_array('fast')),"
            "'sandbox','read-only'))) WHERE name='Helper'"
        )
        conn.commit()
        conn.close()

    def read(self, sql):
        conn = sqlite3.connect(self.store)
        try:
            return conn.execute(sql).fetchall()
        finally:
            conn.close()

    def down(self, *args):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            store_down.main([self.store, *args])
        return json.loads(out.getvalue())

    def test_a_switched_agent_stays_where_it_is_and_each_turn_has_one_node(self):
        self.assertEqual(self.down("110")["from"], 111)
        self.assertEqual(self.read("PRAGMA user_version"), [(110,)])
        self.assertEqual(self.read(f"SELECT id FROM nodes WHERE turn={self.turn}"), [(self.switched,)])
        self.assertEqual(self.read(f"SELECT turn FROM nodes WHERE id={self.original}"), [(None,)])
        self.assertEqual(self.read("SELECT head FROM bots WHERE name='Helper'"), [(self.switched,)])
        self.assertNotIn("family_start", {row[1] for row in self.read("PRAGMA table_info(bots)")})
        self.assertEqual(self.read("SELECT name FROM sqlite_master WHERE name IN ('switches','translated','translating','worded','bots_family_start')"), [])
        self.assertEqual(self.read("SELECT sql LIKE 'CREATE UNIQUE INDEX%' FROM sqlite_master WHERE name='nodes_turn'"),
                         [(1,)])

    def test_back_to_0_10_1_s_shape(self):
        self.assertEqual(self.down("106")["from"], 111)
        self.assertEqual(self.read("PRAGMA user_version"), [(106,)])
        columns = {row[1] for row in self.read("PRAGMA table_info(bots)")}
        self.assertTrue({"instructions", "compaction_instructions"} <= columns)
        self.assertNotIn("profile", columns)
        self.assertEqual(self.read("SELECT name FROM sqlite_master WHERE name='bot_instructions'"), [])
        self.assertEqual(self.read("SELECT DISTINCT instructions FROM bots"), [("Test agent.",)])
        self.assertEqual(self.read("SELECT count(*) FROM bots WHERE json_type(settings,'$.namespace') IS NOT NULL"),
                         [(0,)])
        # A keyed creation's record keeps only what 0.10.1 could have sent.
        self.assertEqual(self.read("SELECT json_extract(creation,'$.request.settings') FROM bots"
                                   " WHERE name='Helper'"), [('{"sandbox":"read-only"}',)])
        lists = self.read("SELECT tools FROM tool_lists")
        self.assertTrue(lists and all('"yields"' not in tools for (tools,) in lists))
        self.assertTrue(any('"lift":true' in tools for (tools,) in lists))
        # Every bot names a list the store keeps.
        self.assertEqual(self.read("SELECT count(*) FROM bots WHERE declared IS NOT NULL"
                                   " AND declared NOT IN (SELECT digest FROM tool_lists)"), [(0,)])
        self.assertEqual(len(glob.glob(self.store + ".schema-111.*.backup")), 1)

    def test_to_0_10_2_only_the_settings_it_refuses_go(self):
        lists = self.read("SELECT * FROM tool_lists")
        self.down("108")
        self.assertEqual(self.read("PRAGMA user_version"), [(108,)])
        self.assertEqual(self.read("SELECT * FROM tool_lists"), lists)
        self.assertNotIn("profile", {row[1] for row in self.read("PRAGMA table_info(bots)")})
        self.assertEqual(self.read("SELECT count(*) FROM bot_instructions"), [(4,)])

    def test_a_dry_run_writes_nothing(self):
        before = Path(self.store).read_bytes()
        self.assertTrue(self.down("106", "--dry-run")["dry_run"])
        self.assertEqual(Path(self.store).read_bytes(), before)
        self.assertEqual(glob.glob(self.store + ".*.backup"), [])

    def test_a_held_store_or_a_schema_out_of_reach_is_refused(self):
        with open(self.store + ".owner-lock", "a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            with self.assertRaisesRegex(SystemExit, "a daemon holds this store"):
                self.down("106")
        for to in ("105", "112"):
            with self.assertRaisesRegex(SystemExit, "goes back to 106 through 111"):
                self.down(to)
        os.link(self.store, self.store + ".alias")
        with self.assertRaisesRegex(SystemExit, "another hard link"):
            self.down("106")
        self.assertEqual(self.read("PRAGMA user_version"), [(111,)])


if __name__ == "__main__":
    unittest.main()
