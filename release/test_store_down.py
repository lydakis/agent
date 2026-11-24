import contextlib
import fcntl
import glob
import hashlib
import io
import json
import os
import sqlite3
import stat
import tempfile
import unittest
from pathlib import Path

import store_down

# The store 0.10.2 made, kept here for the way back: tests/fixtures holds
# only the last two releases' stores, and this one stays as they move on.
FIXTURES = Path(__file__).resolve().parent / "fixtures"

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

UP_112 = """
ALTER TABLE turns ADD COLUMN restarts INTEGER NOT NULL DEFAULT 0;
ALTER TABLE turns ADD COLUMN restart_head INTEGER;
PRAGMA user_version=112;
"""

UP_113 = """
ALTER TABLE nodes ADD COLUMN effort TEXT;
PRAGMA user_version=113;
"""

UP_114 = """
CREATE TABLE texts(digest TEXT PRIMARY KEY, text TEXT NOT NULL, events INTEGER NOT NULL DEFAULT 0);
CREATE INDEX bot_instructions_text ON bot_instructions(instructions);
CREATE INDEX bot_instructions_compaction
    ON bot_instructions(compaction_instructions) WHERE compaction_instructions IS NOT NULL;
ALTER TABLE nodes ADD COLUMN packed INTEGER;
ALTER TABLE bots ADD COLUMN packed_node INTEGER;
"""

# What a configure kept for a turn's end carries, whole, as 113 kept it.
KEPT = {"instructions": "New rules.", "declare": [{"name": "exec", "description": "Run it."}],
        "settings": {"speed": None}}


def packer():
    """zstd as the daemon packs (level 3), or None with no module for it."""
    try:
        from compression import zstd
    except ImportError:
        pass
    else:
        return lambda item: zstd.compress(item, 3)
    try:
        import zstandard
    except ImportError:
        return None
    return zstandard.ZstdCompressor(level=3).compress


def keep(conn, text):
    digest = hashlib.sha256(text.encode()).hexdigest()
    conn.execute("INSERT OR IGNORE INTO texts(digest,text) VALUES (?,?)", (digest, text))
    return digest


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
        # 112 counts each turn's cut-offs; one turn was cut off twice.
        conn.executescript(UP_112)
        conn.execute("UPDATE turns SET restarts=2,restart_head=? WHERE id=?", (self.switched, turn))
        # 113 keeps each prompt's effort on its node; the switched one asked for more.
        conn.executescript(UP_113)
        conn.execute("UPDATE nodes SET effort='high' WHERE id=?", (self.switched,))
        conn.execute(
            "UPDATE bots SET settings=json_set(coalesce(settings,'{}'),'$.namespace','fixture')"
            " WHERE name='Helper'"
        )
        conn.execute(
            "UPDATE bots SET creation=json_object('request',json_object('op','create','settings',"
            "json_object('namespace','fixture','speeds',json_object('openai/*',json_array('fast')),"
            "'sandbox','read-only'))) WHERE name='Helper'"
        )
        # 114 keeps each text bots hold once, by digest, and packs the
        # items of agents idle a week: Helper's longest here.
        self.instructions = dict(conn.execute("SELECT id,instructions FROM bot_instructions"))
        conn.executescript(UP_114)
        for bot, text in self.instructions.items():
            conn.execute("UPDATE bot_instructions SET instructions=? WHERE id=?", (keep(conn, text), bot))
        held = {"instructions": keep(conn, KEPT["instructions"]),
                "declare": keep(conn, json.dumps(KEPT["declare"], separators=(",", ":"), sort_keys=True))}
        stored = {"bot_id": 1, "running_turn": 7, "configure_pending": {"settings": {"speed": None}},
                  "texts": held}
        conn.execute("INSERT INTO events(bot,kind,data) VALUES ('Lead','configure_pending',?)",
                     (json.dumps(stored, separators=(",", ":")),))
        self.event = conn.execute("SELECT max(id) FROM events").fetchone()[0]
        self.packed_node, self.item = conn.execute(
            "SELECT n.id,n.item FROM nodes n JOIN turns t ON t.id=n.turn WHERE t.bot='Helper'"
            " ORDER BY length(n.item) DESC LIMIT 1").fetchone()
        pack = packer()
        if pack:
            conn.execute("UPDATE nodes SET item=?,packed=? WHERE id=?",
                         (pack(bytes(self.item)), len(self.item), self.packed_node))
        conn.execute("PRAGMA user_version=114")
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

    @unittest.skipUnless(packer(), "needs Python 3.14, or the zstandard module")
    def test_packed_messages_and_texts_come_back_whole(self):
        self.assertEqual(self.read(f"SELECT packed FROM nodes WHERE id={self.packed_node}"), [(len(self.item),)])
        self.assertEqual(self.down("113")["from"], 114)
        self.assertEqual(self.read("PRAGMA user_version"), [(113,)])
        self.assertEqual(self.read(f"SELECT item FROM nodes WHERE id={self.packed_node}"), [(self.item,)])
        self.assertNotIn("packed", {row[1] for row in self.read("PRAGMA table_info(nodes)")})
        self.assertNotIn("packed_node", {row[1] for row in self.read("PRAGMA table_info(bots)")})
        self.assertEqual(dict(self.read("SELECT id,instructions FROM bot_instructions")), self.instructions)
        [(data,)] = self.read(f"SELECT data FROM events WHERE id={self.event}")
        self.assertEqual(json.loads(data), {"bot_id": 1, "running_turn": 7, "configure_pending": KEPT})
        self.assertEqual(self.read("SELECT name FROM sqlite_master WHERE name LIKE 'texts%'"
                                   " OR name LIKE 'bot_instructions_%'"), [])

    def test_packed_messages_need_a_way_to_unpack(self):
        if not packer():
            self.skipTest("nothing here packs")
        real = store_down.unpacker
        store_down.unpacker = lambda: None
        try:
            with self.assertRaises(SystemExit) as refused:
                self.down("113")
        finally:
            store_down.unpacker = real
        self.assertIn("packed messages need Python 3.14", str(refused.exception))
        # Taken back as one transaction: the store is as it was.
        self.assertEqual(self.read("PRAGMA user_version"), [(114,)])

    def test_prompts_lose_their_effort(self):
        self.assertEqual(self.down("112")["from"], 114)
        self.assertEqual(self.read("PRAGMA user_version"), [(112,)])
        self.assertNotIn("effort", {row[1] for row in self.read("PRAGMA table_info(nodes)")})
        self.assertEqual(self.read(f"SELECT count(*) FROM nodes WHERE id={self.switched}"), [(1,)])

    def test_turns_lose_their_count_of_cut_offs(self):
        self.assertEqual(self.down("111")["from"], 114)
        self.assertEqual(self.read("PRAGMA user_version"), [(111,)])
        columns = {row[1] for row in self.read("PRAGMA table_info(turns)")}
        self.assertFalse({"restarts", "restart_head"} & columns)
        self.assertEqual(self.read(f"SELECT count(*) FROM turns WHERE id={self.turn}"), [(1,)])

    def test_a_switched_agent_stays_where_it_is_and_each_turn_has_one_node(self):
        self.assertEqual(self.down("110")["from"], 114)
        self.assertEqual(self.read("PRAGMA user_version"), [(110,)])
        self.assertEqual(self.read(f"SELECT id FROM nodes WHERE turn={self.turn}"), [(self.switched,)])
        self.assertEqual(self.read(f"SELECT turn FROM nodes WHERE id={self.original}"), [(None,)])
        self.assertEqual(self.read("SELECT head FROM bots WHERE name='Helper'"), [(self.switched,)])
        self.assertNotIn("family_start", {row[1] for row in self.read("PRAGMA table_info(bots)")})
        self.assertEqual(self.read("SELECT name FROM sqlite_master WHERE name IN ('switches','translated','translating','worded','bots_family_start')"), [])
        self.assertEqual(self.read("SELECT sql LIKE 'CREATE UNIQUE INDEX%' FROM sqlite_master WHERE name='nodes_turn'"),
                         [(1,)])

    def test_back_to_0_10_1_s_shape(self):
        self.assertEqual(self.down("106")["from"], 114)
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
        self.assertEqual(len(glob.glob(self.store + ".schema-114.*.backup")), 1)

    def test_to_0_10_2_only_the_settings_it_refuses_go(self):
        lists = self.read("SELECT * FROM tool_lists")
        self.down("108")
        self.assertEqual(self.read("PRAGMA user_version"), [(108,)])
        self.assertEqual(self.read("SELECT * FROM tool_lists"), lists)
        self.assertNotIn("profile", {row[1] for row in self.read("PRAGMA table_info(bots)")})
        self.assertEqual(self.read("SELECT count(*) FROM bot_instructions"), [(4,)])

    def test_the_copy_has_the_stores_permissions_whatever_the_umask(self):
        # As the daemon's own copy before it migrates: a private store's
        # copy is private, and at no point readable by anyone else.
        umask = os.umask(0o022)
        self.addCleanup(os.umask, umask)
        for mode in (0o600, 0o640):
            with self.subTest(mode=oct(mode)):
                for old in glob.glob(self.store + ".*.backup"):
                    os.unlink(old)
                os.chmod(self.store, mode)
                made = []
                vacuum = store_down.back_up

                def watched(conn, store, copy):
                    real = conn.execute

                    class Conn:
                        def execute(self, sql, *args):
                            if sql.startswith("VACUUM INTO"):
                                made.append(stat.S_IMODE(os.stat(copy).st_mode))
                            return real(sql, *args)
                    return vacuum(Conn(), store, copy)

                store_down.back_up = watched
                try:
                    self.down("111")
                finally:
                    store_down.back_up = vacuum
                [copy] = glob.glob(self.store + ".schema-114.*.backup")
                self.assertEqual(stat.S_IMODE(os.stat(copy).st_mode), mode)
                # Already the store's before the copy is written.
                self.assertEqual(made, [mode])
                self.assertEqual(stat.S_IMODE(os.stat(self.store + ".owner-lock").st_mode), 0o600)
                with sqlite3.connect(copy) as conn:
                    self.assertEqual(conn.execute("PRAGMA user_version").fetchone(), (114,))
                # The next pass starts from 114 again.
                Path(self.store).write_bytes(Path(copy).read_bytes())

    def test_a_copy_that_fails_is_removed(self):
        os.chmod(self.store, 0o600)

        def vacuum(conn, store, copy):
            class Conn:
                def execute(self, sql, *args):
                    raise sqlite3.OperationalError("disk full")
            return back_up(Conn(), store, copy)

        back_up = store_down.back_up
        store_down.back_up = vacuum
        try:
            with self.assertRaisesRegex(sqlite3.OperationalError, "disk full"):
                self.down("111")
        finally:
            store_down.back_up = back_up
        self.assertEqual(glob.glob(self.store + ".*.backup"), [])
        self.assertEqual(self.read("PRAGMA user_version"), [(114,)])

    def test_help_says_how_to_use_it(self):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            store_down.main(["--help"])
        self.assertIn("store_down.py STORE SCHEMA [--dry-run]", out.getvalue())

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
        for to in ("105", "115"):
            with self.assertRaisesRegex(SystemExit, "goes back to 106 through 114"):
                self.down(to)
        os.link(self.store, self.store + ".alias")
        with self.assertRaisesRegex(SystemExit, "another hard link"):
            self.down("106")
        self.assertEqual(self.read("PRAGMA user_version"), [(114,)])


if __name__ == "__main__":
    unittest.main()
