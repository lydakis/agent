"""Provider allowance, permanent quota errors, and billed retry round bounds."""
import http.server
import json
import os
from pathlib import Path
import socket
import tempfile
import threading
import time
import unittest

from bench.runtime_client import Client


class AllowanceModel(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def log_message(self, *_):
        pass

    def setup(self):
        super().setup()
        self.connection.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        self.server.calls += 1
        n, mode = self.server.calls, self.server.mode
        if mode == 'admission' and request['model'] == 'held-model':
            self.server.held.set()
            self.server.release.wait(5)
        if mode == 'quota' and n == 1:
            payload = json.dumps({'error': {self.server.quota_field: 'insufficient_quota',
                                 'message': 'Check billing.'}}).encode()
            self.send_response(429)
            self.send_header('Content-Type', 'application/json')
            # This must not block the next bot in the same pool.
            self.send_header('Retry-After', '60')
            self.send_header('x-ratelimit-limit-tokens', '1000')
            self.send_header('x-ratelimit-remaining-tokens', '0')
        else:
            if mode.startswith('rounds') and n % 2 == (1 if mode == 'rounds' else 0):
                events = [{'type': 'response.failed', 'response': {
                    'error': {'code': 'rate_limit_exceeded', 'message': 'Please try again in 0s.'},
                    'usage': {'input_tokens': 5, 'output_tokens': 5}}}]
            else:
                output = [{'type': 'message', 'role': 'assistant',
                           'content': [{'type': 'output_text', 'text': 'ok'}]}]
                if mode.startswith('rounds'):
                    output = [{'type': 'function_call', 'name': 'echo', 'call_id': f'call-{n}',
                               'arguments': '{"text":"ok"}'}]
                events = [{'type': 'response.completed', 'response': {
                    'status': 'completed', 'output': output,
                    'usage': {'input_tokens': 5, 'output_tokens': 5}}}]
                if not mode.startswith('rounds'):
                    events.insert(0, {'type': 'response.output_text.delta', 'delta': 'ok'})
            payload = ''.join('data: '+json.dumps(e)+'\n\n' for e in events).encode()
            self.send_response(200)
            self.send_header('Content-Type', 'text/event-stream')
            if mode in ('pacing', 'streaming', 'admission'):
                self.send_header('x-ratelimit-limit-tokens', '1000')
                self.send_header('x-ratelimit-remaining-tokens', str(1000 - n * 10))
        self.send_header('Content-Length', str(len(payload)))
        self.end_headers()
        if mode == 'streaming' and n == 1:
            first, rest = payload.split(b'\n\n', 1)
            self.wfile.write(first + b'\n\n')
            self.wfile.flush()
            self.server.release.wait(5)
            payload = rest
        self.wfile.write(payload)
        self.wfile.flush()


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'requires built Rust runtime')
class ProviderPacingTests(unittest.TestCase):
    def setUp(self):
        root = Path(__file__).resolve().parent.parent
        directory = tempfile.TemporaryDirectory(dir=root/'.local')
        self.addCleanup(directory.cleanup)
        self.path = Path(directory.name)
        self.server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), AllowanceModel)
        self.server.daemon_threads = True
        self.server.calls = 0
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)
        self.binary = root/'.local/target/release/agent'

    def client(self, suffix='', extra=()):
        client = Client(self.binary, self.path/f'store{suffix}.sqlite',
                        f'http://127.0.0.1:{self.server.server_port}/v1',
                        extra=('--max-output-tokens', '800') + extra)
        self.addCleanup(client.close, kill=True)
        return client

    def submit(self, client, bot):
        self.assertIn('result', client.request('create', bot=bot, workspace=str(self.path), instructions='x'))
        return client.request('submit', bot=bot, request_id='run', prompt='hello')['result']['turn']

    def test_fresh_token_headers_leave_unused_estimates_available(self):
        self.server.mode = 'pacing'
        client = self.client()
        for bot in ('A', 'B', 'C', 'D'):
            turn = self.submit(client, bot)
            self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertEqual(self.server.calls, 4)

    def test_reported_allowance_admits_other_calls_before_stream_completion(self):
        self.server.mode = 'streaming'
        self.server.release = threading.Event()
        client = self.client()
        self.addCleanup(self.server.release.set)
        first = self.submit(client, 'A')
        client.receive(lambda e: e.get('event') == 'text_delta' and e.get('turn') == first)
        second = self.submit(client, 'B')
        done = client.receive(lambda e: e.get('event') == 'turn_finished'
                              and e.get('turn') == second, timeout=1)
        self.assertEqual(done['data']['status'], 'completed')
        self.assertFalse(any(e.get('event') == 'turn_finished' and e.get('turn') == first
                             for e in client.saved))
        self.server.release.set()
        self.assertEqual(client.finished(first)['data']['status'], 'completed')

    def test_cancelled_startup_wait_does_not_consume_provider_allowance(self):
        self.server.mode = 'admission'
        self.server.held = threading.Event()
        self.server.release = threading.Event()
        client = self.client(extra=('--max-connecting', '1'))
        self.addCleanup(self.server.release.set)
        warm = self.submit(client, 'Warm')
        self.assertEqual(client.finished(warm)['data']['status'], 'completed')
        self.assertIn('result', client.request('create', bot='Held', workspace=str(self.path),
                                               instructions='x', model='openai/held-model'))
        held = client.request('submit', bot='Held', request_id='run', prompt='hello')['result']['turn']
        self.assertTrue(self.server.held.wait(2))
        cancelled = self.submit(client, 'Cancelled')
        # Let the accepted turn reach the occupied startup semaphore.
        time.sleep(.1)
        client.request('interrupt', bot='Cancelled', turn=cancelled)
        self.assertEqual(client.finished(cancelled)['data']['status'], 'interrupted')
        self.assertEqual(self.server.calls, 2)
        self.server.release.set()
        self.assertEqual(client.finished(held)['data']['status'], 'completed')
        following = self.submit(client, 'Next')
        done = client.receive(lambda e: e.get('event') == 'turn_finished'
                              and e.get('turn') == following, timeout=1)
        self.assertEqual(done['data']['status'], 'completed')
        self.assertEqual(self.server.calls, 3)

    def test_permanent_quota_fails_once_without_pausing_the_pool(self):
        self.server.mode = 'quota'
        for field in ('type', 'code'):
            with self.subTest(field=field):
                self.server.calls = 0
                self.server.quota_field = field
                client = self.client(field)
                turn = self.submit(client, 'A')
                self.assertEqual(client.finished(turn)['data']['error'], 'provider_quota_exhausted')
                row = client.request('turns', bot='A')['result']['turns'][0]
                self.assertEqual((row['retries'], row['model_rounds']), (0, 0))
                self.assertEqual(self.server.calls, 1)
                turn = self.submit(client, 'B')
                self.assertEqual(client.finished(turn)['data']['status'], 'completed')
                self.assertEqual(self.server.calls, 2)
                client.close()

    def test_billed_retries_share_the_durable_turn_round_limit(self):
        # Exercise the limit ending on a successful tool plan and on a billed
        # failure. Committed tools still run; no 201st request reaches the model.
        for mode, retries in (('rounds', 100), ('rounds-failure', 99)):
            with self.subTest(mode=mode):
                self.server.calls = 0
                self.server.mode = mode
                client = self.client(mode)
                turn = self.submit(client, 'A')
                done = client.receive(lambda e: e.get('event') == 'turn_finished'
                                      and e.get('turn') == turn, timeout=30)
                self.assertEqual(done['data']['error'], 'tool_round_limit')
                row = client.request('turns', bot='A')['result']['turns'][0]
                self.assertEqual((row['model_rounds'], row['input_tokens'], row['output_tokens'], row['retries']),
                                 (200, 1000, 1000, retries))
                self.assertEqual(self.server.calls, 200)
                events, after = [], 0
                while True:
                    page = client.request('events', bot='A', after=after, limit=256)['result']
                    if not page['events']:
                        break
                    events.extend(page['events'])
                    after = page['next_cursor']
                self.assertEqual(sum(e['event'] == 'tool_completed' for e in events), 100)
                client.close()
