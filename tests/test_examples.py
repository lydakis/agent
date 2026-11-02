"""The example apps in examples/ still work against this build, as an adopter would run them."""
import json
import os
import subprocess
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class ExampleTests(unittest.TestCase):
    def test_the_triage_app_triages_every_issue_through_its_declared_tools(self):
        env = dict(os.environ, AGENT_BIN=str(ROOT / '.local/target/release/agent'))
        done = subprocess.run([str(ROOT / 'examples/triage/offline.sh')], env=env,
                              capture_output=True, text=True, timeout=120)
        self.assertEqual(done.returncode, 0, done.stderr)
        decisions = {d['issue']: d for d in map(json.loads, done.stdout.splitlines())}
        self.assertEqual(sorted(decisions), [101, 102, 103, 104, 105])
        self.assertEqual(decisions[102]['duplicate_of'], 101)
        self.assertEqual(done.stderr.count(': completed: '), 5, done.stderr)


if __name__ == '__main__':
    unittest.main()
