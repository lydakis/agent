"""A scripted OpenAI Responses endpoint for demoing Sonny without an API key.

A coordinator (its instructions carry the coordinator role) hands the work
to a task; any other bot runs the same small task: run the tests, fix the
bug with `edit`, run them again, then stream a markdown summary.
Usage: python3 model.py PORT
With SONNY_DEMO_WAIT=1 the lead waits on the task and reports, so the task's
window closes when it is done.
"""
import http.server, json, os, re, sys, time

WAIT = os.environ.get('SONNY_DEMO_WAIT') == '1'

SUMMARY = """Fixed the off-by-one in `average`.

## What changed
- `calc.py`: divide by `len(values)`, not `len(values) - 1`
- The test suite passes again (**3 passed**)

```python
def average(values):
    return sum(values) / len(values)
```

Nothing else in the folder needed touching."""

# Ids are unique per request: a steer restarts the script, and a turn keys its
# calls by id.
def call(name, call_id, **arguments):
    return {'type': 'function_call', 'name': name, 'call_id': call_id, 'arguments': json.dumps(arguments)}

class Model(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'
    def log_message(self, *_): pass
    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        items = request['input']
        last_user = max(i for i, item in enumerate(items) if item.get('role') == 'user')
        step = sum(item.get('type') == 'function_call_output' for item in items[last_user:])
        text, output = '', []
        lead = '# Role: coordinator' in request.get('instructions', '')
        if lead and step == 0:
            text = "That needs a fix and a test run, so I'll hand it to a task."
            output = [call('shell', f'l1-{len(items)}', command='"$AGENT_BIN" run --detach --new --bot "${AGENT_BOT%.lead}.fix-avg" '
                           '--model "$AGENT_MODEL" -- "Fix the failing test"')]
        elif lead and WAIT and step == 1:
            # The handle `run --detach` printed, waited on: the task's window
            # closes once the wait returns its finished turn.
            started = next(item['output'] for item in reversed(items) if item.get('type') == 'function_call_output')
            handle = re.search(r'turn:[\w.-]+/\d+', started).group(0)
            text = "Waiting for `fix-avg` to finish."
            output = [call('wait', f'w1-{len(items)}', handles=[handle])]
        elif lead and WAIT:
            text = 'Done: `fix-avg` fixed the average and the tests pass.'
        elif lead:
            text = ('Started `fix-avg`. It has its own window in Rex, '
                    'so you can watch it or step in there.')
        elif step == 0:
            text = "I'll run the tests first to see what fails."
            output = [call('shell', f's1-{len(items)}', command='python3 test_calc.py')]
        elif step == 1:
            text = 'The average divides by one too few. Fixing it.'
            output = [call('edit', f'e1-{len(items)}', path='calc.py', old='    return sum(values) / (len(values) - 1)\n',
                           new='    return sum(values) / len(values)\n')]
        elif step == 2:
            output = [call('shell', f's2-{len(items)}', command='python3 test_calc.py')]
        else:
            text = SUMMARY
        if text:
            output.insert(0, {'type': 'message', 'role': 'assistant',
                              'content': [{'type': 'output_text', 'text': text}]})
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Transfer-Encoding', 'chunked')
        self.end_headers()
        def send(event):
            frame = ('data: ' + json.dumps(event) + '\n\n').encode()
            self.wfile.write(f'{len(frame):x}\r\n'.encode() + frame + b'\r\n')
            self.wfile.flush()
        send({'type': 'response.created', 'response': {'id': 'r'}})
        time.sleep(0.6)
        for start in range(0, len(text), 4):
            send({'type': 'response.output_text.delta', 'delta': text[start:start + 4]})
            time.sleep(0.025)
        send({'type': 'response.completed', 'response': {
            'model': request['model'], 'status': 'completed', 'output': output,
            'usage': {'input_tokens': 1200 + 300 * step, 'output_tokens': 40 + len(text) // 4,
                      'input_tokens_details': {'cached_tokens': 1000 if step else 0}}}})
        self.wfile.write(b'0\r\n\r\n')
        self.wfile.flush()

server = http.server.ThreadingHTTPServer(('127.0.0.1', int(sys.argv[1])), Model)
server.daemon_threads = True
server.serve_forever()
