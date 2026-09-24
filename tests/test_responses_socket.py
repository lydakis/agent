"""Responses over WebSocket: one connection per bot, and delta input after the
first call. The fixture is a minimal RFC 6455 server, since the test
environment has no WebSocket library."""
import base64
import hashlib
import json
import os
from pathlib import Path
import socketserver
import struct
import tempfile
import threading
import time
import unittest

from bench.runtime_client import Client

GUID = b'258EAFA5-E914-47DA-95CA-C5AB0DC85B11'


class Socket(socketserver.BaseRequestHandler):
    """Answers a user message with an echo call, and a tool result with text."""

    def read_exact(self, size):
        data = b''
        while len(data) < size:
            chunk = self.request.recv(size - len(data))
            if not chunk:
                raise ConnectionError
            data += chunk
        return data

    def frame(self):
        head = self.read_exact(2)
        opcode, size = head[0] & 0x0F, head[1] & 0x7F
        if size == 126:
            size = struct.unpack('>H', self.read_exact(2))[0]
        elif size == 127:
            size = struct.unpack('>Q', self.read_exact(8))[0]
        mask = self.read_exact(4) if head[1] & 0x80 else b'\0\0\0\0'
        data = bytes(b ^ mask[i % 4] for i, b in enumerate(self.read_exact(size)))
        return opcode, data

    def send(self, event):
        data = json.dumps(event).encode()
        size = len(data)
        head = bytes([0x81]) + (bytes([size]) if size < 126 else
                                bytes([126]) + struct.pack('>H', size) if size < 65536 else
                                bytes([127]) + struct.pack('>Q', size))
        self.request.sendall(head + data)

    def handle(self):
        request = b''
        while b'\r\n\r\n' not in request:
            request += self.request.recv(4096)
        headers = dict(line.split(': ', 1) for line in request.decode().split('\r\n')[1:] if ': ' in line)
        headers = {k.lower(): v for k, v in headers.items()}
        with self.server.lock:
            refuse = self.server.refuse > 0
            self.server.refuse -= refuse
        if refuse:
            self.request.sendall(b'HTTP/1.1 429 Too Many Requests\r\nRetry-After: 1\r\n'
                                 b'Content-Length: 0\r\n\r\n')
            return
        accept = base64.b64encode(hashlib.sha1(headers['sec-websocket-key'].encode() + GUID).digest())
        # A small request allowance, so the test can count paced requests.
        self.request.sendall(b'HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n'
                             b'Connection: Upgrade\r\nSec-WebSocket-Accept: ' + accept + b'\r\n'
                             b'x-ratelimit-limit-requests: 6\r\nx-ratelimit-remaining-requests: 6\r\n\r\n')
        with self.server.lock:
            self.server.connections.append(headers)
            connection = len(self.server.connections) - 1
        try:
            while True:
                opcode, data = self.frame()
                if opcode == 8:
                    return
                if opcode != 1:
                    continue
                body = json.loads(data)
                with self.server.lock:
                    self.server.requests.append((connection, body))
                    number = len(self.server.requests)
                    forget = number in self.server.forget
                if forget and body.get('previous_response_id'):
                    self.send({'type': 'error', 'status': 400, 'error': {
                        'type': 'invalid_request_error', 'code': 'previous_response_not_found',
                        'message': f"Previous response with id '{body['previous_response_id']}' not found."}})
                    continue
                if body['input'][-1].get('type') == 'function_call_output':
                    item = {'type': 'message', 'role': 'assistant', 'id': f'msg_{number}',
                            'content': [{'type': 'output_text', 'text': 'done'}]}
                    self.send({'type': 'response.output_text.delta', 'delta': 'done'})
                else:
                    item = {'type': 'function_call', 'id': f'fc_{number}', 'call_id': f'call_{number}',
                            'name': 'echo', 'arguments': json.dumps({'text': 'hi'})}
                self.send({'type': 'response.output_item.done', 'item': item})
                self.send({'type': 'response.completed', 'response': {
                    'id': f'resp_{number}', 'status': 'completed', 'output': [item],
                    'usage': {'input_tokens': 10, 'output_tokens': 2}}})
        except (ConnectionError, OSError):
            pass


def serve(test, forget=(), refuse=0):
    server = socketserver.ThreadingTCPServer(('127.0.0.1', 0), Socket)
    server.daemon_threads = True
    server.lock, server.connections, server.requests = threading.Lock(), [], []
    server.forget, server.refuse = set(forget), refuse
    threading.Thread(target=server.serve_forever, daemon=True).start()
    test.addCleanup(server.server_close)
    test.addCleanup(server.shutdown)
    return server


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'requires built Rust runtime')
class ResponsesSocketTests(unittest.TestCase):
    def test_a_rate_limited_upgrade_waits_out_its_retry_after(self):
        root = Path(__file__).resolve().parent.parent
        server = serve(self, refuse=1)
        with tempfile.TemporaryDirectory(dir=root/'.local') as directory:
            client = Client(root/'.local/target/release/agent', Path(directory)/'agent.db',
                            f'http://127.0.0.1:{server.server_address[1]}/v1', family='responses-ws')
            self.addCleanup(client.close)
            client.request('create', bot='Bob', workspace=directory)
            started = time.monotonic()
            turn = client.request('submit', bot='Bob', request_id='r', prompt='hello')['result']['turn']
            retry = client.receive(lambda e: e.get('event') == 'retry' and e.get('turn') == turn)
            self.assertEqual(retry['error'], 'provider_http_429')
            self.assertEqual(client.finished(turn)['data']['status'], 'completed')
            # The pool stayed closed for the upgrade's retry-after.
            self.assertGreaterEqual(time.monotonic() - started, 0.9)
            self.assertEqual(len(server.connections), 1)

    def test_a_bot_continues_on_its_connection_and_resends_in_full_when_the_server_forgot(self):
        root = Path(__file__).resolve().parent.parent
        # The fourth request is turn two's tool result; the server has lost it.
        server = serve(self, forget={4})
        with tempfile.TemporaryDirectory(dir=root/'.local') as directory:
            # One startup permit: the full resend must take it again, not hold it.
            client = Client(root/'.local/target/release/agent', Path(directory)/'agent.db',
                            f'http://127.0.0.1:{server.server_address[1]}/v1', family='responses-ws',
                            extra=['--max-connecting', '1'])
            self.addCleanup(client.close)
            client.request('create', bot='Bob', workspace=directory)
            for prompt in ('hello', 'again'):
                turn = client.request('submit', bot='Bob', request_id=prompt, prompt=prompt)['result']['turn']
                self.assertEqual(client.finished(turn)['data']['status'], 'completed')
            requests = server.requests
            self.assertEqual(len(server.connections), 1)
            self.assertEqual(server.connections[0].get('openai-beta'), 'responses_websockets=2026-02-06')
            self.assertEqual({connection for connection, _ in requests}, {0})
            bodies = [body for _, body in requests]
            self.assertTrue(all(b['type'] == 'response.create' and 'stream' not in b for b in bodies))
            self.assertEqual([b.get('previous_response_id') for b in bodies],
                             [None, 'resp_1', 'resp_2', 'resp_3', None])
            # The first call carries the prompt; each continuation only what is new.
            self.assertEqual(len(bodies[0]['input']), 1)
            self.assertEqual([i.get('type') for i in bodies[1]['input']], ['function_call_output'])
            self.assertEqual([i.get('role') for i in bodies[2]['input']], ['user'])
            self.assertEqual([i.get('type') for i in bodies[3]['input']], ['function_call_output'])
            # The forgotten continuation is sent again with the whole history.
            full = bodies[4]['input']
            self.assertEqual([i.get('type') or i.get('role') for i in full],
                             ['user', 'function_call', 'function_call_output', 'message',
                              'user', 'function_call', 'function_call_output'])
            self.assertEqual(full[-1], bodies[3]['input'][0])
            # The idle connection is kept for Bob's next call and counted.
            stats = client.request('stats')['result']
            self.assertEqual(stats['providers']['openai']['sockets'], 1)
            # All five requests, the refused continuation and its full resend
            # included, were paced against the allowance of six.
            (pool,) = stats['providers']['openai']['pools'].values()
            self.assertEqual(pool['requests_available'], 1)
            turns = client.request('turns', bot='Bob')['result']['turns']
            self.assertEqual([t['retries'] for t in turns], [0, 0])


if __name__ == '__main__':
    unittest.main()
