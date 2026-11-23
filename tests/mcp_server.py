"""A stand-in stdio MCP server for Sonny's tests: `echo`, `add` (with
structured output), `fail` (a tool error) and `slow`. Each start appends
its pid to $MCP_STARTS when that is set, and waits $MCP_START_MS before it
answers; calls are answered at once, each on a thread of its own, and each
call's tool name is appended to $MCP_CALLS when that is set, as it
arrives: what reached the server, whatever happens to the answer.

With $MCP_EVENTS set it offers the MCP events extension: `ci.failed`, polled,
whose events are the lines of the file $MCP_EVENTS names, the cursor how many
it read (each poll's params appended to $MCP_POLLS when set); and
`comment.created`, pushed, which takes a `document_id` and tells one comment
on it at once (`bad` is refused). A request of the extension that does not
say the client takes it is refused, as the extension says."""
import json
import os
import sys
import threading
import time

if os.environ.get('MCP_STARTS'):
    with open(os.environ['MCP_STARTS'], 'a') as f:
        f.write(f'{os.getpid()}\n')

time.sleep(int(os.environ.get('MCP_START_MS', '0')) / 1000)

TOOLS = [
    {'name': 'echo', 'description': 'Echo text back.',
     'inputSchema': {'type': 'object', 'properties': {'text': {'type': 'string'}}, 'required': ['text']},
     'annotations': {'readOnlyHint': True}},
    {'name': 'add', 'description': 'Add two numbers.',
     'inputSchema': {'type': 'object', 'properties': {'a': {'type': 'number'}, 'b': {'type': 'number'}},
                     'required': ['a', 'b']},
     'outputSchema': {'type': 'object', 'properties': {'sum': {'type': 'number'}}, 'required': ['sum']}},
    {'name': 'fail', 'description': 'Always fails.', 'inputSchema': {'type': 'object'}},
    {'name': 'slow', 'description': 'Sleeps for ms milliseconds, or until the file until names exists.',
     'inputSchema': {'type': 'object', 'properties': {'ms': {'type': 'number'}, 'until': {'type': 'string'}}}},
]


EVENTS = os.environ.get('MCP_EVENTS')
EXTENSION = 'io.modelcontextprotocol/events'
OFFERED = [
    {'name': 'ci.failed', 'description': 'A CI run failed.', 'delivery': ['poll'],
     'inputSchema': {'type': 'object', 'properties': {'repo': {'type': 'string'}}},
     'payloadSchema': {'type': 'object'}},
    {'name': 'comment.created', 'description': 'A comment on a document.', 'delivery': ['push', 'poll'],
     'inputSchema': {'type': 'object', 'properties': {'document_id': {'type': 'string'}},
                     'required': ['document_id']},
     'payloadSchema': {'type': 'object'}},
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
        while arguments.get('until') and not os.path.exists(arguments['until']):
            time.sleep(0.01)
        return {'content': [{'type': 'text', 'text': 'slept'}]}
    return {'content': [{'type': 'text', 'text': 'it failed'}], 'isError': True}


for line in sys.stdin:
    message = json.loads(line)
    method, request = message.get('method'), message.get('id')
    if request is None:
        continue
    if method == 'initialize':
        capabilities = {'tools': {}}
        if EVENTS:
            capabilities['extensions'] = {EXTENSION: {}}
        send({'jsonrpc': '2.0', 'id': request, 'result': {
            'protocolVersion': message['params'].get('protocolVersion', '2025-06-18'),
            'capabilities': capabilities, 'serverInfo': {'name': 'test', 'version': '0'},
            'instructions': 'A test server.'}})
    elif EVENTS and method.startswith('events/'):
        params = message.get('params') or {}
        meta = params.get('_meta', {}).get('io.modelcontextprotocol/clientCapabilities', {})
        if EXTENSION not in meta.get('extensions', {}):
            send({'jsonrpc': '2.0', 'id': request, 'error': {'code': -32601, 'message': 'Method not found'}})
        elif method == 'events/list':
            send({'jsonrpc': '2.0', 'id': request, 'result': {'events': OFFERED}})
        elif method == 'events/poll':
            if os.environ.get('MCP_POLLS'):
                with open(os.environ['MCP_POLLS'], 'a') as f:
                    f.write(json.dumps({k: v for k, v in params.items() if k != '_meta'}) + '\n')
            lines = open(EVENTS).read().splitlines() if os.path.exists(EVENTS) else []
            # No cursor starts from now: nothing of the past.
            start = len(lines) if params.get('cursor') is None else int(params['cursor'])
            events = [{'name': params['name'], 'timestamp': '2026-11-23T03:00:00Z', **json.loads(line)}
                      for line in lines[start:]]
            send({'jsonrpc': '2.0', 'id': request, 'result': {
                'events': events, 'cursor': str(len(lines)), 'truncated': False, 'hasMore': False,
                'nextPollMs': 50}})
        elif method == 'events/stream':
            document = (params.get('arguments') or {}).get('document_id')
            if document == 'bad':
                send({'jsonrpc': '2.0', 'id': request, 'error': {'code': -32602, 'message': 'no such document'}})
                continue
            meta = {'io.modelcontextprotocol/subscriptionId': request}
            send({'jsonrpc': '2.0', 'method': 'notifications/events/active', 'params': {
                'cursor': 's0', 'truncated': False, 'heartbeatMs': 60000, '_meta': meta}})
            send({'jsonrpc': '2.0', 'method': 'notifications/events/event', 'params': {
                'eventId': f'c-{document}', 'name': 'comment.created', 'timestamp': '2026-11-23T03:00:00Z',
                'data': {'document_id': document, 'text': 'looks good'}, 'cursor': f's1-{document}',
                '_meta': meta}})
    elif method == 'tools/list':
        send({'jsonrpc': '2.0', 'id': request, 'result': {'tools': TOOLS}})
    elif method == 'tools/call':
        # Each call on a thread of its own, so calls overlap.
        params = message['params']
        if os.environ.get('MCP_CALLS'):
            with open(os.environ['MCP_CALLS'], 'a') as f:
                f.write(f"{params['name']}\n")
        threading.Thread(target=lambda request=request, params=params: send(
            {'jsonrpc': '2.0', 'id': request, 'result': result(params['name'], params.get('arguments') or {})}),
            daemon=True).start()
    else:
        send({'jsonrpc': '2.0', 'id': request, 'result': {}})
