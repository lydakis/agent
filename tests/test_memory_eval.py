import io
import json
import os
import tempfile
import threading
import unittest
from pathlib import Path

from bench import memory_eval, synthetic_model
from bench.targets import clean_env

ROOT = Path(__file__).resolve().parents[1]


class ReferenceTests(unittest.TestCase):
    def test_hidden_checks_pass_the_right_answer_and_fail_the_wrong_one(self):
        memory_eval.self_check()


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class PlumbingTests(unittest.TestCase):
    """The whole eval against the synthetic model, which answers without
    tools: no model is paid, every task is scored wrong, and what the bot
    was told shows whether memory reached its instructions."""

    def setUp(self):
        self.binary = ROOT / '.local/target/release/agent'
        self.app = Path(os.environ.get('AGENT_TEST_APP', ROOT / '.local/target/debug/agent-app'))
        if not self.app.exists():
            self.skipTest('build the app (cargo build in app/src-tauri) or set AGENT_TEST_APP')
        self.seen = []
        lock = threading.Lock()
        reply = synthetic_model.Model.reply
        seen = self.seen

        def recording(handler):
            # Read the body once, keep its instructions, and hand it on.
            length = int(handler.headers['Content-Length'])
            body = handler.rfile.read(length)
            with lock:
                seen.append(json.loads(body).get('instructions', ''))
            handler.rfile = io.BytesIO(body)
            return reply(handler)
        synthetic_model.Model.reply = recording
        self.addCleanup(setattr, synthetic_model.Model, 'reply', reply)
        self.server, self.url = synthetic_model.start()
        self.addCleanup(self.server.shutdown)

    def test_memory_reaches_a_new_task_in_a_worktree_only_in_the_memory_condition(self):
        env = {**clean_env(), 'AGENT_PROVIDER': f'synth=responses,{self.url},SYNTH_KEY', 'SYNTH_KEY': 'k'}
        with tempfile.TemporaryDirectory() as out:
            rows = {}
            for condition in memory_eval.CONDITIONS:
                self.seen.clear()
                rows[condition] = memory_eval.run_bot(self.binary, self.app, 'synth/model', env, Path(out),
                                                      condition, 'decision', 0, 100_000, 120)
                rows[condition]['instructions'] = list(self.seen)
        for condition, row in rows.items():
            self.assertEqual(row['status'], 'completed', row)
            self.assertGreaterEqual(row['model_rounds'], 1)
            self.assertFalse(row['check']['correct'])
            self.assertTrue(row['visible_tests_pass'])
            self.assertTrue(row['answer'])
            self.assertTrue(row['instructions'])
        told = rows['memory']['instructions'][0]
        self.assertIn('api-timestamps', told)
        self.assertIn('error-codes', told)
        self.assertIn('/projects/shop', told)
        self.assertNotIn('# Memory in', rows['none']['instructions'][0])
        self.assertEqual(rows['memory']['memory_saved'], [])


if __name__ == '__main__':
    unittest.main()
