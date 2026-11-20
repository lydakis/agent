import contextlib
import fcntl
import hashlib
import io
import json
import os
import sqlite3
import tempfile
import unittest

import store_lift_yields as script


def dumps(value):
    return json.dumps(value, separators=(",", ":"), ensure_ascii=False)


def digest(text):
    return hashlib.sha256(text.encode()).hexdigest()


def tool(name, **rest):
    # In the daemon's field order: `lift`/`yields` before `kind`.
    out = {"name": name, "description": "A synthetic tool.", "parameters": {"type": "object"}}
    out.update(rest)
    return out


EXEC = [tool("exec", answered_by="sonny", lift=True, kind="exec"), tool("margin")]
QUICK = [tool("react", answered_by="sonny")]


class StoreLiftYields(unittest.TestCase):
    def setUp(self):
        folder = tempfile.mkdtemp()
        self.path = os.path.join(folder, "state.sqlite")
        conn = sqlite3.connect(self.path)
        conn.executescript(
            "CREATE TABLE tool_lists(digest TEXT PRIMARY KEY, tools TEXT NOT NULL);"
            "CREATE TABLE bots(id INTEGER PRIMARY KEY, name TEXT, declared TEXT,"
            " creation TEXT, configure_pending TEXT);"
            "PRAGMA user_version=107;"
        )
        self.exec_text, self.quick_text = dumps(EXEC), dumps(QUICK)
        self.exec_digest, self.quick_digest = digest(self.exec_text), digest(self.quick_text)
        conn.executemany(
            "INSERT INTO tool_lists VALUES (?,?)",
            [(self.exec_digest, self.exec_text), (self.quick_digest, self.quick_text)],
        )
        made = dumps({"request": {"op": "create", "declare": self.exec_digest}})
        kept = dumps({"declare": EXEC, "model": "m"})
        conn.executemany(
            "INSERT INTO bots VALUES (?,?,?,?,?)",
            [
                (1, "lead", self.exec_digest, made, None),
                (2, "lead.task", self.exec_digest, None, kept),
                (3, "quiet", self.quick_digest, None, None),
                (4, "bare", None, None, None),
            ],
        )
        conn.commit()
        conn.close()

    def run_script(self, *args):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            script.main([*args, self.path])
        return json.loads(out.getvalue())

    def rows(self):
        conn = sqlite3.connect(self.path)
        try:
            lists = dict(conn.execute("SELECT digest,tools FROM tool_lists"))
            bots = {
                row[0]: row[1:]
                for row in conn.execute("SELECT name,declared,creation,configure_pending FROM bots")
            }
            return lists, bots
        finally:
            conn.close()

    def test_up_renames_lift_and_moves_every_holder_to_the_new_digest(self):
        report = self.run_script("up")
        self.assertEqual(
            (report["lists"], report["bots"], report["creations"], report["configures"]),
            (1, 2, 1, 1),
        )
        self.assertEqual(report["tools_still_lift"], 0)
        lists, bots = self.rows()
        renamed = [tool("exec", answered_by="sonny", yields=True, kind="exec"), tool("margin")]
        text = dumps(renamed)
        # The key keeps its place, so the bytes are the daemon's own.
        self.assertEqual(text, self.exec_text.replace('"lift":true', '"yields":true'))
        self.assertEqual(lists, {digest(text): text, self.quick_digest: self.quick_text})
        self.assertEqual(bots["lead"][0], digest(text))
        self.assertEqual(json.loads(bots["lead"][1])["request"]["declare"], digest(text))
        self.assertEqual(bots["lead.task"][0], digest(text))
        self.assertEqual(json.loads(bots["lead.task"][2])["declare"], renamed)
        self.assertEqual(bots["quiet"][0], self.quick_digest)
        self.assertIsNone(bots["bare"][0])

    def test_down_undoes_up_byte_for_byte(self):
        before = self.rows()
        self.run_script("up")
        self.assertNotEqual(self.rows(), before)
        report = self.run_script("down")
        self.assertEqual(report["tools_still_yields"], 0)
        self.assertEqual(self.rows(), before)

    def test_a_second_run_changes_nothing(self):
        self.run_script("up")
        after = self.rows()
        report = self.run_script("up")
        self.assertEqual(report["lists"] + report["bots"] + report["configures"], 0)
        self.assertEqual(self.rows(), after)

    def test_every_byte_but_the_key_stays_as_the_daemon_wrote_it(self):
        # serde_json writes 1e-7 where Python would write 1e-07, and keeps
        # an escape Python would not make.
        text = (
            '[{"name":"exec","description":"a \\u00e9 \\"q\\"","parameters":'
            '{"type":"number","minimum":1e-7,"maximum":1.0E10,"lift":true},'
            '"answered_by":"sonny","lift":true,"kind":"browser"},{"name":"b","lift":false}]'
        )
        up = script.renamed_text(text, "lift", "yields")
        self.assertEqual(
            up,
            text.replace('"answered_by":"sonny","lift"', '"answered_by":"sonny","yields"')
            .replace('{"name":"b","lift"', '{"name":"b","yields"'),
        )
        self.assertEqual(script.renamed_text(up, "yields", "lift"), text)
        self.assertIsNone(script.renamed_text(up, "lift", "yields"))
        self.assertIsNone(script.renamed_text("[]", "lift", "yields"))

    def test_a_store_a_daemon_holds_is_refused(self):
        before = self.rows()
        with open(self.path + ".owner-lock", "a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            with self.assertRaisesRegex(SystemExit, "a daemon holds this store"):
                self.run_script("up")
        os.link(self.path, self.path + ".alias")
        with self.assertRaisesRegex(SystemExit, "another hard link"):
            self.run_script("up")
        self.assertEqual(self.rows(), before)

    def test_dry_run_writes_nothing(self):
        before = self.rows()
        report = self.run_script("up", "--dry-run")
        self.assertEqual(report["lists"], 1)
        self.assertEqual(self.rows(), before)


if __name__ == "__main__":
    unittest.main()
