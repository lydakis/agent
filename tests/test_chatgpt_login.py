"""The chatgpt provider reads Codex's saved ChatGPT login only for its own endpoint."""
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

    def serve(self, directory, spec):
        return subprocess.run(
            [str(self.binary), 'serve', '--store', str(Path(directory) / 'state.db'), '--provider', spec],
            env={**clean_env(), 'CODEX_HOME': str(Path(directory) / 'codex')},
            stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=10)

    def test_a_missing_login_stops_startup(self):
        with tempfile.TemporaryDirectory(dir=self.root / '.local') as directory:
            process = self.serve(directory, 'chatgpt')
        self.assertNotEqual(process.returncode, 0)
        self.assertIn('provider_login_unavailable', process.stderr)
        self.assertIn(str(Path(directory) / 'codex' / 'auth.json'), process.stderr)

    def test_another_endpoint_never_reads_the_login(self):
        with tempfile.TemporaryDirectory(dir=self.root / '.local') as directory:
            process = self.serve(directory, 'chatgpt=responses,http://127.0.0.1:9/backend-api/codex')
        self.assertEqual(process.returncode, 0, process.stderr)
        self.assertNotIn('provider_login_unavailable', process.stderr)


if __name__ == '__main__':
    unittest.main()
