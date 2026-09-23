"""The chatgpt provider signs requests with Codex's saved ChatGPT login."""
import http.server
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import unittest

from bench.runtime_client import Client, serve_args
from bench.targets import clean_env

TOKEN = 'synthetic-access-token'
ACCOUNT = 'synthetic-account'


class Backend(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        self.rfile.read(int(self.headers['Content-Length']))
        self.server.seen.append((self.path, self.headers.get('Authorization'),
                                 self.headers.get('ChatGPT-Account-ID')))
        output = [{'type': 'message', 'role': 'assistant',
                   'content': [{'type': 'output_text', 'text': 'ok'}]}]
        body = ''.join('data: ' + json.dumps(e) + '\n\n' for e in [
            {'type': 'response.output_text.delta', 'delta': 'ok'},
            {'type': 'response.completed', 'response': {'status': 'completed', 'output': output,
             'usage': {'input_tokens': 3, 'output_tokens': 2}}}]).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'requires built Rust runtime')
class ChatgptLoginTests(unittest.TestCase):
    root = Path(__file__).resolve().parent.parent
    binary = root / '.local/target/release/agent'

    def test_requests_carry_the_login_token_and_workspace(self):
        server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Backend)
        server.seen = []
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        with tempfile.TemporaryDirectory(dir=self.root / '.local') as directory:
            codex = Path(directory) / 'codex'
            codex.mkdir()
            (codex / 'auth.json').write_text(json.dumps({
                'OPENAI_API_KEY': None, 'last_refresh': '2026-09-23T00:00:00Z',
                'tokens': {'id_token': 'x', 'access_token': TOKEN, 'refresh_token': 'r',
                           'account_id': ACCOUNT}}))
            client = Client(self.binary, Path(directory) / 'state.db',
                            f'http://127.0.0.1:{server.server_port}/backend-api/codex',
                            provider='chatgpt', env={**clean_env(), 'CODEX_HOME': str(codex)})
            try:
                client.request('create', bot='Bob', workspace=directory)
                turn = client.request('submit', bot='Bob', request_id='r', prompt='hi')['result']['turn']
                self.assertEqual(client.finished(turn)['data']['status'], 'completed')
            finally:
                client.close()
        self.assertEqual(server.seen, [('/backend-api/codex/responses', f'Bearer {TOKEN}', ACCOUNT)])

    def test_a_missing_login_stops_startup(self):
        with tempfile.TemporaryDirectory(dir=self.root / '.local') as directory:
            codex = Path(directory) / 'codex'
            process = subprocess.run(
                [str(self.binary), *serve_args(Path(directory) / 'state.db', 'http://127.0.0.1:9',
                                               provider='chatgpt')],
                env={**clean_env(), 'CODEX_HOME': str(codex)}, stdin=subprocess.DEVNULL,
                capture_output=True, text=True, timeout=10)
        self.assertNotEqual(process.returncode, 0)
        self.assertIn('provider_login_unavailable', process.stderr)
        self.assertIn(str(codex / 'auth.json'), process.stderr)


if __name__ == '__main__':
    unittest.main()
