"""The stores the last releases wrote open, run and go back down.

Each tests/fixtures/store-vX.Y.Z.sql is the synthetic store that release's
daemon made (release/make_store_fixture.py). The daemon under test opens
it, so every step from its schema runs on what that release wrote; every
agent lists and inspects, the turn it left parked ends, the message queued
behind it runs, and new turns run on its declared tools.
release/store_down.py then takes it back to the release's schema, and the
release's own daemon opens it and runs turns. A stored shape changed with no step for it fails
here, before a release ships it.
"""
import json
import os
import sqlite3
import subprocess
import sys
import unittest
from pathlib import Path

from bench.runtime_client import Client
from release.make_store_fixture import WORKSPACE, respond
from release.release_binary import fetch
from tests.test_runtime import ModelFixture

ROOT = Path(__file__).resolve().parent.parent
FIXTURES = sorted((ROOT / 'tests' / 'fixtures').glob('store-v*.sql'))
BOTS = {'Lead', 'Lead.task', 'Helper', 'Helper.fork'}


def released(tag):
    """`tag`'s daemon, fetched into AGENT_RELEASE_BINARIES; None without it,
    except in CI, which always checks the release reads the store."""
    folder = os.environ.get('AGENT_RELEASE_BINARIES')
    if not folder:
        if os.environ.get('GITHUB_ACTIONS'):
            raise AssertionError('set AGENT_RELEASE_BINARIES: CI opens each store with its release')
        return None
    return fetch(tag, folder)


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class ReleaseStoreTests(ModelFixture):
    def setUp(self):
        super().setUp()
        self.model.respond = respond
        Path(WORKSPACE).mkdir(parents=True, exist_ok=True)

    def open(self, binary, store):
        client = Client(binary, store, self.url, 'echo,shell')
        self.addCleanup(client.close)
        return client

    def runs(self, client, by):
        """Every agent lists and inspects; a script turn on the lead's
        declared `exec` and a plain one on the helper complete."""
        listing = client.request('bots')
        self.assertIn('result', listing, listing)
        listed = {bot['name'] for bot in listing['result']['bots']}
        self.assertEqual(listed, BOTS)
        for bot in sorted(BOTS):
            self.assertIn('result', client.request('inspect', bot=bot), bot)
        turn = client.request('submit', bot='Lead', request_id=f'{by}-script', prompt='run a script')['result']['message']
        client.receive(lambda m: m.get('event') == 'tool_started' and m.get('turn') == turn, 10)
        self.assertIn('result', client.request('tool_result', bot='Lead', turn=turn, call_id='exec-1', output='ok'))
        self.assertEqual(client.finished(turn, 10)['data']['status'], 'completed')
        turn = client.request('submit', bot='Helper', request_id=f'{by}-hello', prompt='hello')['result']['message']
        self.assertEqual(client.finished(turn, 10)['data']['status'], 'completed')

    def test_each_release_s_store_opens_runs_and_goes_back(self):
        self.assertGreaterEqual(len(FIXTURES), 1)
        for fixture in FIXTURES:
            with self.subTest(fixture=fixture.name):
                self.check(fixture)

    def check(self, fixture):
        tag = fixture.stem.removeprefix('store-')
        store = self.path / tag / 'state.sqlite'
        store.parent.mkdir()
        with sqlite3.connect(store) as conn:
            conn.executescript(fixture.read_text())
            schema = conn.execute('PRAGMA user_version').fetchone()[0]
            parked, queued = [row[0] for row in conn.execute(
                "SELECT id FROM messages WHERE bot='Lead' ORDER BY id DESC LIMIT 2")][::-1]
        conn.close()

        client = self.open(self.binary, store)
        current = client.ready['schema']
        self.assertGreaterEqual(current, schema)
        inspected = client.request('inspect', bot='Lead')
        self.assertIn('result', inspected, inspected)
        declared = inspected['result']['declared']
        self.assertEqual([tool['name'] for tool in declared], ['exec', 'browser_open', 'desktop_click', 'note'])
        self.assertTrue(declared[0]['yields'])
        # The call the release left waiting is answered now, or has
        # expired; either way its turn ends and the queued message runs.
        [view] = client.request('turns', bot='Lead', newest=True, limit=1)['result']['turns']
        if view.get('waiting_on'):
            client.request('tool_result', bot='Lead', turn=parked, call_id='exec-1', output='late')
        handles = [f'Lead/{parked}', f'Lead/{queued}']
        results = client.request('wait', handles=handles, timeout_ms=15000)['result']['results']
        self.assertEqual([results[h].get('status') for h in handles], ['completed', 'completed'], results)
        self.runs(client, 'new')
        client.close()

        down = subprocess.run([sys.executable, ROOT / 'release' / 'store_down.py', store, str(schema)],
                              capture_output=True, text=True)
        self.assertEqual(down.returncode, 0, down.stderr)
        self.assertEqual(json.loads(down.stdout)['schema'], schema)
        self.assertEqual(json.loads(down.stdout)['from'], current)

        binary = released(tag)
        if binary is None:
            return
        old = self.open(binary, store)
        self.assertEqual(old.ready['schema'], schema)
        self.runs(old, 'old')
        old.close()
