"""A scripted Responses endpoint for recording the README demo without an API key.

Only the model's words are scripted. The daemon, the store, the bots, and every
tool call (shell, edit, wait, and the `agent run --detach` a bot uses to
delegate) run for real against the files in `tally/`. Each bot's script is
chosen by its latest prompt and advanced by the tool results it has received.

    python3 scripted_model.py PORT

Port 0 picks a free port. The bound port is printed on the first line.
"""
import http.server
import json
import re
import sys
import time

TESTS = "python3 -m unittest 2>&1 | grep -E 'Error|FAILED|OK'"
DELEGATE = 'agent run --detach --new --bot docs -- "Check that README.md matches what tally.py does"'


def call(name, **arguments):
    return ('call', name, arguments)


# Each step is (text, tool call or None). The last step of a script has no call.
# `lead` waits for the helper before it edits tally.py, since both work in the
# same directory and the helper must see the file unfixed.
SCRIPTS = {
    'Fix the failing test': [
        ("I'll have a helper check the README while I run the tests.", call('shell', command=DELEGATE)),
        ('', call('shell', command=TESTS)),
        ("Before I change the code, I'll hear from the helper:", call('wait', handles='HANDLE')),
        ('Both point at `int()`, which truncates 28.999… to 28 cents. Rounding instead:',
         call('edit', path='tally.py', old='int(p * 100)', new='round(p * 100)')),
        ('', call('shell', command=TESTS)),
        ('Fixed: `total()` truncated each price to whole cents instead of rounding '
         '(tally.py:3). All 3 tests pass. The helper confirmed the README already describes '
         'rounding, so only the code changed.', None),
    ],
    'Check that README.md matches': [
        ('', call('shell', command='python3 -c "from tally import total; print(total([0.29, 0.1]))"')),
        ('README expects 0.39 for [0.29, 0.1]; tally.py returns 0.38 (int() truncates).', None),
    ],
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
        results = [item for item in items[users[-1]:] if item.get('type') == 'function_call_output']
        script = next((s for key, s in SCRIPTS.items() if prompt.startswith(key)), None)
        if script is None:
            text, tool = f'No script for: {prompt}', None
        else:
            text, tool = script[min(len(results), len(script) - 1)]
        output = []
        if text:
            output.append({'type': 'message', 'role': 'assistant',
                           'content': [{'type': 'output_text', 'text': text}]})
        if tool:
            _, name, arguments = tool
            if arguments.get('handles') == 'HANDLE':
                # The handle `run --detach` printed in the first tool result.
                handle = re.search(r'turn:[\w.-]+/\d+', results[0]['output']).group(0)
                arguments = {'handles': [handle]}
            output.append({'type': 'function_call', 'name': name, 'call_id': f'call-{len(results)}',
                           'arguments': json.dumps(arguments)})
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Transfer-Encoding', 'chunked')
        self.end_headers()
        self.frame({'type': 'response.created', 'response': {'id': 'r'}})
        time.sleep(0.4)
        # Stream a few words at a time, at a readable pace.
        for word in re.findall(r'\S+\s*', text):
            self.frame({'type': 'response.output_text.delta', 'delta': word})
            time.sleep(0.035)
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
