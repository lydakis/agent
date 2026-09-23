"""A provider stream that only sends keepalives is retried, not held forever."""
import http.server
import json
import os
from pathlib import Path
import tempfile
import threading
import time
import unittest

from bench.runtime_client import Client


class Stalling(http.server.BaseHTTPRequestHandler):
    """Anthropic Messages: the first call starts, then only pings; later calls finish."""
    protocol_version = 'HTTP/1.1'

    def log_message(self, *_):
        pass

    def send(self, frame):
        self.wfile.write(f'{len(frame):x}\r\n'.encode() + frame + b'\r\n')
        self.wfile.flush()

    def event(self, kind, **body):
        self.send(f'event: {kind}\ndata: {json.dumps({"type": kind, **body})}\n\n'.encode())

    def do_POST(self):
        self.rfile.read(int(self.headers['Content-Length']))
        with self.server.lock:
            self.server.calls += 1
            stall = self.server.calls == 1
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Transfer-Encoding', 'chunked')
        self.end_headers()
        try:
            self.event('message_start', message={'usage': {'input_tokens': 5}})
            self.event('content_block_start', index=0, content_block={'type': 'text', 'text': ''})
            if stall:
                # Keepalives well inside the bound, until the client gives up.
                while True:
                    self.event('ping')
                    self.send(b': keepalive\n\n')
                    time.sleep(.2)
            self.event('content_block_delta', index=0, delta={'type': 'text_delta', 'text': 'done'})
            self.event('content_block_stop', index=0)
            self.event('message_delta', delta={'stop_reason': 'end_turn'}, usage={'output_tokens': 1})
            self.event('message_stop')
            self.wfile.write(b'0\r\n\r\n')
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'requires built Rust runtime')
class ProviderStallTests(unittest.TestCase):
    def test_a_stream_of_only_keepalives_fails_the_attempt_and_the_retry_completes(self):
        root = Path(__file__).resolve().parent.parent
        server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Stalling)
        server.daemon_threads = True
        server.calls, server.lock = 0, threading.Lock()
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        with tempfile.TemporaryDirectory(dir=root/'.local') as directory:
            client = Client(root/'.local/target/release/agent', Path(directory)/'agent.db',
                            f'http://127.0.0.1:{server.server_port}/v1', family='anthropic',
                            provider='fixture', extra=['--stall-timeout', '1'])
            self.addCleanup(client.close)
            client.request('create', bot='Bob', workspace=directory)
            started = time.monotonic()
            turn = client.request('submit', bot='Bob', request_id='r', prompt='hello')['result']['turn']
            retry = client.receive(lambda e: e.get('event') == 'retry' and e.get('turn') == turn)
            self.assertEqual((retry['attempt'], retry['error']), (1, 'provider_stream_stalled'))
            # The bound runs from the last content frame, not the last byte.
            self.assertGreaterEqual(time.monotonic() - started, 1)
            finished = client.finished(turn)
            self.assertEqual(finished['data']['status'], 'completed')
            self.assertEqual(server.calls, 2)
            self.assertEqual(client.request('turns', bot='Bob')['result']['turns'][0]['retries'], 1)
