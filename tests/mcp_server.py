"""A stand-in stdio MCP server for Sonny's tests: `echo`, `add` (with
structured output), `fail` (a tool error) and `slow`. Each start appends
its pid to $MCP_STARTS when that is set; calls are answered at once,
each on a thread of its own."""
import json
import os
import sys
import threading
import time

if os.environ.get('MCP_STARTS'):
    with open(os.environ['MCP_STARTS'], 'a') as f:
        f.write(f'{os.getpid()}\n')

TOOLS = [
    {'name': 'echo', 'description': 'Echo text back.',
     'inputSchema': {'type': 'object', 'properties': {'text': {'type': 'string'}}, 'required': ['text']},
     'annotations': {'readOnlyHint': True}},
    {'name': 'add', 'description': 'Add two numbers.',
     'inputSchema': {'type': 'object', 'properties': {'a': {'type': 'number'}, 'b': {'type': 'number'}},
                     'required': ['a', 'b']},
     'outputSchema': {'type': 'object', 'properties': {'sum': {'type': 'number'}}, 'required': ['sum']}},
    {'name': 'fail', 'description': 'Always fails.', 'inputSchema': {'type': 'object'}},
    {'name': 'slow', 'description': 'Sleeps for ms milliseconds.',
     'inputSchema': {'type': 'object', 'properties': {'ms': {'type': 'number'}}}},
]


WRITING = threading.Lock()


def send(message):
    with WRITING:
        sys.stdout.write(json.dumps(message) + '\n')
        sys.stdout.flush()


def result(name, arguments):
    if name == 'echo':
        return {'content': [{'type': 'text', 'text': arguments['text']}]}
    if name == 'add':
        total = arguments['a'] + arguments['b']
        return {'content': [{'type': 'text', 'text': str(total)}], 'structuredContent': {'sum': total}}
    if name == 'slow':
        time.sleep(arguments.get('ms', 0) / 1000)
        return {'content': [{'type': 'text', 'text': 'slept'}]}
    return {'content': [{'type': 'text', 'text': 'it failed'}], 'isError': True}


for line in sys.stdin:
    message = json.loads(line)
    method, request = message.get('method'), message.get('id')
    if request is None:
        continue
    if method == 'initialize':
        send({'jsonrpc': '2.0', 'id': request, 'result': {
            'protocolVersion': message['params'].get('protocolVersion', '2025-06-18'),
            'capabilities': {'tools': {}}, 'serverInfo': {'name': 'test', 'version': '0'},
            'instructions': 'A test server.'}})
    elif method == 'tools/list':
        send({'jsonrpc': '2.0', 'id': request, 'result': {'tools': TOOLS}})
    elif method == 'tools/call':
        # Each call on a thread of its own, so calls overlap.
        params = message['params']
        threading.Thread(target=lambda request=request, params=params: send(
            {'jsonrpc': '2.0', 'id': request, 'result': result(params['name'], params.get('arguments') or {})}),
            daemon=True).start()
    else:
        send({'jsonrpc': '2.0', 'id': request, 'result': {}})
