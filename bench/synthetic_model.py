"""An instant synthetic Responses endpoint for daemon-shape screens.

Replies to any prompt with a short message. `hold:` prompts block until the
server's `release` event is set, which lets a screen park many bots on one
anchor turn. `shell:` prompts return one shell tool call and then a reply; `wait:` prompts
return one wait tool call on the given handles and then a reply; `delay:N`
prompts reply after N milliseconds so many turns overlap; `limited:` prompts
are refused with 429 and a one-second Retry-After every time.
No history validation: the daemon's request bodies are counted, not checked.
"""
import http.server
import json
import threading
import time


class Model(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def log_message(self, *_):
        pass

    def handle(self):
        try:
            super().handle()
        except (BrokenPipeError, ConnectionResetError):
            pass  # includes the next keep-alive read after a deliberate kill

    def do_POST(self):
        self.reply()

    def reply(self):
        length = int(self.headers['Content-Length'])
        request = json.loads(self.rfile.read(length))
        self.server.requests += 1
        self.server.request_bytes += length
        user = [i for i in request['input'] if i.get('role') == 'user'][-1]['content'][0]['text']
        last = request['input'][-1]
        if user.startswith('limited:'):
            # A provider that never lets this turn through: 429 with a short
            # Retry-After, so its pool stays closed and the turn keeps waiting.
            body = json.dumps({'error': {'message': 'try later'}}).encode()
            self.send_response(429)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(body)))
            self.send_header('Retry-After', '1')
            self.end_headers()
            self.wfile.write(body)
            return
        held = user.startswith('hold:')
        delay = float(user[6:]) / 1000 if user.startswith('delay:') else 0
        if user.startswith('shell:') and last.get('type') != 'function_call_output':
            text, output = '', [{'type': 'function_call', 'name': 'shell', 'call_id': 'sh-1',
                                 'arguments': json.dumps({'command': user[6:], 'timeout_ms': 5000})}]
        elif user.startswith('wait:') and last.get('type') != 'function_call_output':
            text, output = '', [{'type': 'function_call', 'name': 'wait', 'call_id': 'w-1',
                                 'arguments': json.dumps({'handles': user[5:].split(',')})}]
        else:
            text = 'done'
            output = [{'type': 'message', 'role': 'assistant', 'content': [{'type': 'output_text', 'text': text}]}]
        events = [{'type': 'response.created', 'response': {'id': 'r'}},
                  *([{'type': 'response.output_text.delta', 'delta': text}] if text else []),
                  {'type': 'response.completed', 'response': {'status': 'completed', 'output': output,
                   'usage': {'input_tokens': 1, 'output_tokens': 1, 'input_tokens_details': {'cached_tokens': 0}}}}]
        frames = [('data: ' + json.dumps(e) + '\n\n').encode() for e in events]
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Transfer-Encoding', 'chunked')
        self.end_headers()
        if held or delay:
            # Keep the stream open like a provider that pings while it thinks:
            # a comment line every few seconds until the screen releases it,
            # or a fixed delay that makes many turns overlap.
            self.chunk(frames[0])
            if delay:
                time.sleep(delay)
            else:
                while not self.server.release.wait(5):
                    self.chunk(b': hold\n\n')
            frames = frames[1:]
        for frame in frames:
            self.chunk(frame)
        self.wfile.write(b'0\r\n\r\n')
        self.wfile.flush()

    def chunk(self, data):
        self.wfile.write(f'{len(data):x}\r\n'.encode() + data + b'\r\n')
        self.wfile.flush()


def start():
    """A running server; `server.release` unblocks held prompts."""
    class Server(http.server.ThreadingHTTPServer):
        request_queue_size = 1024
        daemon_threads = True
    server = Server(('127.0.0.1', 0), Model)
    server.requests, server.request_bytes = 0, 0
    server.release = threading.Event()
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, f'http://127.0.0.1:{server.server_port}/v1'
