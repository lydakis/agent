"""The chatgpt provider reads the plan login `agent login chatgpt` saved, only for its own endpoint."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

from bench.targets import clean_env


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'requires built Rust runtime')
class ChatgptLoginTests(unittest.TestCase):
    root = Path(__file__).resolve().parent.parent
    binary = root / '.local/target/release/agent'

    def run_agent(self, directory, *args):
        return subprocess.run(
            [str(self.binary), *args],
            env={**clean_env(), 'AGENT_CHATGPT_LOGIN': str(Path(directory) / 'chatgpt.json')},
            stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=10)

    def serve(self, directory, spec):
        return self.run_agent(directory, 'serve', '--store', str(Path(directory) / 'state.db'),
                              '--provider', spec)

    def test_a_missing_login_stops_startup_and_says_how_to_sign_in(self):
        with tempfile.TemporaryDirectory(dir=self.root / '.local') as directory:
            process = self.serve(directory, 'chatgpt')
        self.assertNotEqual(process.returncode, 0)
        self.assertIn('provider_login_unavailable', process.stderr)
        self.assertIn(str(Path(directory) / 'chatgpt.json'), process.stderr)
        self.assertIn('agent login chatgpt', process.stderr)

    def test_a_login_that_may_not_spend_the_plan_stops_startup(self):
        with tempfile.TemporaryDirectory(dir=self.root / '.local') as directory:
            (Path(directory) / 'chatgpt.json').write_text(json.dumps({
                'client_id': 'oaiapp_x', 'access_token': 'a', 'refresh_token': 'r',
                'id_token': '', 'expires_at': 1, 'scope': 'openid profile email'}))
            process = self.serve(directory, 'chatgpt')
        self.assertNotEqual(process.returncode, 0)
        self.assertIn('provider_login_unavailable', process.stderr)
        self.assertIn('chatgpt.tokens.use.direct', process.stderr)

    def test_another_endpoint_never_reads_the_login(self):
        with tempfile.TemporaryDirectory(dir=self.root / '.local') as directory:
            process = self.serve(directory, 'chatgpt=responses,http://127.0.0.1:9/v1')
        self.assertEqual(process.returncode, 0, process.stderr)
        self.assertNotIn('provider_login_unavailable', process.stderr)

    def test_login_names_its_provider(self):
        with tempfile.TemporaryDirectory(dir=self.root / '.local') as directory:
            for args in (['login'], ['login', 'openai'], ['login', 'chatgpt', '--port', '0']):
                process = self.run_agent(directory, *args)
                self.assertEqual(process.returncode, 2, (args, process.stderr))
                self.assertIn('"usage"', process.stderr)
            self.assertFalse((Path(directory) / 'chatgpt.json').exists())


if __name__ == '__main__':
    unittest.main()
