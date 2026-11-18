"""Scripted Responses endpoint: a reply streamed slowly, so the window types while it streams."""
import http.server, json, sys, time

REPLY = ("I counted every tally in the ledger against the receipts folder. "
         "Most rows line up; three weeks show a gap where a refund was booked twice. "
         "I am checking each of those weeks now, one receipt at a time, and will "
         "list the rows that differ with the amount and the receipt they point to. ") * 3

class Model(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'
    def log_message(self, *_): pass
    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        items = request['input']
        last_user = max(i for i, item in enumerate(items) if item.get('role') == 'user')
        asked = str(items[last_user].get('content'))
        text = REPLY if 'tallies' in asked else 'Noted, I will look at that next.'
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Transfer-Encoding', 'chunked')
        self.end_headers()
        def send(event):
            frame = ('data: ' + json.dumps(event) + '\n\n').encode()
            self.wfile.write(f'{len(frame):x}\r\n'.encode() + frame + b'\r\n')
            self.wfile.flush()
        send({'type': 'response.created', 'response': {'id': 'r'}})
        for start in range(0, len(text), 6):
            send({'type': 'response.output_text.delta', 'delta': text[start:start + 6]})
            time.sleep(0.12)
        output = [{'type': 'message', 'role': 'assistant', 'content': [{'type': 'output_text', 'text': text}]}]
        send({'type': 'response.completed', 'response': {
            'model': request['model'], 'status': 'completed', 'output': output,
            'usage': {'input_tokens': 1200, 'output_tokens': 40 + len(text) // 4,
                      'input_tokens_details': {'cached_tokens': 1000}}}})
        self.wfile.write(b'0\r\n\r\n')
        self.wfile.flush()

server = http.server.ThreadingHTTPServer(('127.0.0.1', int(sys.argv[1])), Model)
print(server.server_address[1], flush=True)
server.daemon_threads = True
server.serve_forever()
