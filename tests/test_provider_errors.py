"""Provider diagnostics preserve useful errors without retaining configured keys."""
import http.server
import json
import os
from pathlib import Path
import sqlite3
import tempfile
import threading
import unittest

from bench.runtime_client import Client
from bench.targets import clean_env

KEY = 'synthetic-"quoted-\\credential/suffix'


class Errors(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        self.rfile.read(int(self.headers['Content-Length']))
        mode = self.server.mode
        message = 'problem: '+KEY+' / '+KEY
        status, content_type = 200, 'text/event-stream'
        if mode.startswith('http_'):
            status, content_type = 401, 'application/json'
            if mode == 'http_json':
                body = json.dumps({'error': {'message': message}})
            elif mode == 'http_unknown_json':
                body = json.dumps({'unexpected': {'message': message}})
            elif mode == 'http_root_json':
                body = '"'+''.join('\\u%04x' % ord(c) for c in message)+'"'
            elif mode == 'http_plain':
                content_type, body = 'text/plain', message
            elif mode == 'http_plain_benign':
                content_type, body = 'text/plain', 'quota exhausted'
            elif mode == 'http_escaped_plain':
                body = '{"error": "problem: '+json.dumps(KEY)[1:-1].replace('/', '\\/')  # Incomplete JSON, complete body.
            elif mode == 'http_oversized':
                body = ' '*4090+KEY
            elif mode == 'http_broken':
                body = KEY[:10]
            else:
                body = json.dumps({'error': {'message': 'quota exhausted'}})
        else:
            if mode == 'responses_cutoff':
                event = {'type':'response.failed', 'response':{'error':{'message':'x'*500+KEY}}}
            elif mode == 'responses_quoted_key':
                event = {'type':'response.failed', 'response':{'error':{'message':json.dumps(KEY)}}}
            elif mode == 'responses_reason':
                event = {'type':'response.incomplete', 'response':{'incomplete_details':{'reason':message}}}
            elif mode == 'responses_error':
                event = {'type':'error', 'message':message}
            elif mode == 'anthropic_stop':
                events = [{'type':'message_delta','delta':{'stop_reason':message}}, {'type':'message_stop'}]
                event = None
            else:
                event = {'type':'error','error':{'message':message}}
            if event is not None:
                events = [event]
            body = ''.join('data: '+json.dumps(event)+'\n\n' for event in events)
        body = body.encode()
        self.send_response(status)
        self.send_header('Content-Type', content_type)
        self.send_header('Content-Length', str(len(body)+(100 if mode == 'http_broken' else 0)))
        self.end_headers()
        self.wfile.write(body)
        self.close_connection = True


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'requires built Rust runtime')
class ProviderErrorTests(unittest.TestCase):
    def test_http_and_stream_errors_are_sanitized_before_delivery_and_persistence(self):
        root = Path(__file__).resolve().parent.parent
        server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Errors)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        cases = [('responses', mode) for mode in ('http_json', 'http_escaped_plain',
                 'http_oversized', 'http_broken', 'http_benign', 'http_root_json',
                 'http_plain', 'http_plain_benign', 'http_unknown_json', 'responses_error',
                 'responses_cutoff', 'responses_reason', 'responses_quoted_key')]
        cases += [('anthropic', mode) for mode in ('http_json', 'anthropic_error', 'anthropic_stop')]
        with tempfile.TemporaryDirectory(dir=root/'.local') as directory:
            for n, (family, mode) in enumerate(cases):
                with self.subTest(family=family, mode=mode):
                    server.mode = mode
                    path = Path(directory)/f'{n}.db'
                    client = Client(root/'.local/target/release/agent', path,
                                    f'http://127.0.0.1:{server.server_port}/v1',
                                    family=family, provider='fixture', key_env='AGENT_ERROR_TEST_KEY',
                                    env={**clean_env(), 'AGENT_ERROR_TEST_KEY':KEY})
                    try:
                        client.request('create', bot='Bob', workspace=directory)
                        turn = client.request('submit', bot='Bob', request_id='r', prompt='check')['result']['turn']
                        finished = client.finished(turn)
                        detail = finished['data']['detail']
                        expected_code = 'provider_http_401' if mode.startswith('http_') else (
                            'provider_error' if mode == 'anthropic_error' else 'provider_incomplete')
                        self.assertEqual(finished['data']['error'], expected_code)
                        if mode in ('http_oversized', 'http_broken', 'http_escaped_plain', 'http_unknown_json'):
                            self.assertIsNone(detail)
                        elif mode in ('http_benign', 'http_plain_benign'):
                            self.assertEqual(detail, 'quota exhausted')
                        else:
                            self.assertNotIn(KEY[:10], detail)
                            self.assertNotIn(json.dumps(KEY)[1:-1], detail)
                            self.assertIn('[REDACTED]', detail)
                            self.assertLessEqual(len(detail), 512)
                        page = client.request('events', bot='Bob', after=0, limit=256)['result']['events']
                        terminal = next(e for e in page if e['event'] == 'turn_finished')
                        self.assertEqual(terminal['data']['detail'], detail)
                        with sqlite3.connect(path) as db:
                            stored = json.loads(db.execute("SELECT data FROM events WHERE kind='turn_finished'").fetchone()[0])
                            self.assertEqual(stored['detail'], detail)
                    finally:
                        client.close()
