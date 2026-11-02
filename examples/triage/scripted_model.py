"""A scripted Responses endpoint, so the example runs without an API key.

Only the model's words are scripted: the daemon, the store, the forks, and
both declared tools run for real, answered by triage.py. Each issue's script
is picked by the number in its prompt and advanced by the tool results the
turn has received so far.

    python3 scripted_model.py PORT    # 0 picks a free port, printed first
"""
import http.server
import json
import re
import sys

# number: (search query, set_triage arguments, final reply)
DECISIONS = {
    101: ('empty config', {'labels': ['bug'], 'priority': 'p1',
                           'summary': 'Empty config file panics in config parsing.'},
          'Bug, p1: an empty config panics; #102 reports the same crash.'),
    102: ('empty config', {'labels': ['bug', 'duplicate'], 'priority': 'p1', 'duplicate_of': 101,
                           'summary': 'Same empty-config panic as #101.'},
          'Duplicate of #101, which reports the same panic first.'),
    103: ('theme', {'labels': ['feature'], 'priority': 'p3',
                    'summary': 'Dark theme for the HTML report.'},
          'Feature request, p3: nothing earlier asks for a theme.'),
    104: ('install', {'labels': ['docs'], 'priority': 'p2',
                      'summary': 'README names the wrong crate to install.'},
          'Docs, p2: the README install line names the wrong crate.'),
    105: ('token', {'labels': ['bug', 'security'], 'priority': 'p0',
                    'summary': 'Debug logging leaks the API token.'},
          'Security bug, p0: the debug log prints the API token.'),
}


class Model(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def log_message(self, *_):
        pass

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        items = request['input']
        users = [i for i, item in enumerate(items) if item.get('role') == 'user']
        prompt = items[users[-1]]['content'][0]['text']
        answered = sum(1 for item in items[users[-1]:] if item.get('type') == 'function_call_output')
        query, triage, reply = DECISIONS[int(re.search(r'#(\d+)', prompt).group(1))]
        output = []
        if answered == 0:
            call = ('search_issues', {'query': query})
        elif answered == 1:
            call = ('set_triage', triage)
        else:
            call = None
            output.append({'type': 'message', 'role': 'assistant',
                           'content': [{'type': 'output_text', 'text': reply}]})
        if call:
            output.append({'type': 'function_call', 'name': call[0], 'call_id': f'call-{answered}',
                           'arguments': json.dumps(call[1])})
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Transfer-Encoding', 'chunked')
        self.end_headers()
        self.frame({'type': 'response.created', 'response': {'id': 'r'}})
        if call is None:
            self.frame({'type': 'response.output_text.delta', 'delta': reply})
        self.frame({'type': 'response.completed', 'response': {
            'status': 'completed', 'output': output,
            'usage': {'input_tokens': 0, 'output_tokens': 0, 'input_tokens_details': {'cached_tokens': 0}}}})
        self.wfile.write(b'0\r\n\r\n')
        self.wfile.flush()

    def frame(self, event):
        data = f'data: {json.dumps(event)}\n\n'.encode()
        self.wfile.write(f'{len(data):x}\r\n'.encode() + data + b'\r\n')
        self.wfile.flush()


if __name__ == '__main__':
    server = http.server.ThreadingHTTPServer(('127.0.0.1', int(sys.argv[1])), Model)
    print(server.server_port, flush=True)
    server.serve_forever()
