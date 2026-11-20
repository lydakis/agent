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
        conn.execute("PRAGMA user_version=109")
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

    def test_back_to_0_10_1_s_shape(self):
        self.assertEqual(self.down("106")["from"], 109)
        self.assertEqual(self.read("PRAGMA user_version"), [(106,)])
        columns = {row[1] for row in self.read("PRAGMA table_info(bots)")}
        self.assertTrue({"instructions", "compaction_instructions"} <= columns)
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
        self.assertEqual(len(glob.glob(self.store + ".schema-109.*.backup")), 1)

    def test_to_0_10_2_only_the_settings_it_refuses_go(self):
        lists = self.read("SELECT * FROM tool_lists")
        self.down("108")
        self.assertEqual(self.read("PRAGMA user_version"), [(108,)])
        self.assertEqual(self.read("SELECT * FROM tool_lists"), lists)
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
        for to in ("105", "110"):
            with self.assertRaisesRegex(SystemExit, "goes back to 106 through 109"):
                self.down(to)
        os.link(self.store, self.store + ".alias")
        with self.assertRaisesRegex(SystemExit, "another hard link"):
            self.down("106")
        self.assertEqual(self.read("PRAGMA user_version"), [(109,)])


if __name__ == "__main__":
    unittest.main()
