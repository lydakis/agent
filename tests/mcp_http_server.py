"""A stand-in streamable HTTP MCP server for Sonny's tests that keeps
state per session, as MCP lets a server: one tool, `page`, which pages a
log of $MCP_PAGES pages (5 unset), each page's `cursor` known only to the
session that was given it; a cursor another session gives back is refused
with `ExpiredCursor`, as a remote log's server did. It serves on a free
port of 127.0.0.1 and prints that port as its first line. Each session it
opens is appended to $MCP_SESSIONS when that is set."""
import http.server
import json
import os
import sys
import threading
import uuid

PAGES = int(os.environ.get('MCP_PAGES', '5'))
TOOLS = [{'name': 'page', 'description': 'One page of the log, from a cursor the last page gave.',
          'inputSchema': {'type': 'object', 'properties': {'cursor': {'type': 'string'}}},
          'annotations': {'readOnlyHint': True}}]

LOCK = threading.Lock()
# Each session's cursors: the page each one leads to.
SESSIONS = {}


def answer(session, message):
    method, request = message.get('method'), message.get('id')
    params = message.get('params') or {}
    if method == 'initialize':
        return {'protocolVersion': params.get('protocolVersion', '2025-06-18'),
                'capabilities': {'tools': {}}, 'serverInfo': {'name': 'log', 'version': '0'}}
    if method == 'tools/list':
        return {'tools': TOOLS}
    if method == 'tools/call':
        cursor = (params.get('arguments') or {}).get('cursor')
        with LOCK:
            cursors = SESSIONS[session]
            if cursor is None:
                page = 1
            elif cursor in cursors:
                page = cursors.pop(cursor)
            else:
                return {'content': [{'type': 'text', 'text': f'ExpiredCursor: {cursor}'}], 'isError': True}
            following = None
            if page < PAGES:
                following = uuid.uuid4().hex
                cursors[following] = page + 1
        body = {'page': page, 'cursor': following}
        return {'content': [{'type': 'text', 'text': json.dumps(body)}], 'structuredContent': body}
    return {}


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def log_message(self, *args):
        pass

    def reply(self, status, body=None, headers=()):
        data = b'' if body is None else json.dumps(body).encode()
        self.send_response(status)
        for name, value in headers:
            self.send_header(name, value)
        if body is not None:
            self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        self.reply(405)

    def do_DELETE(self):
        with LOCK:
            SESSIONS.pop(self.headers.get('Mcp-Session-Id'), None)
        self.reply(200)

    def do_POST(self):
        message = json.loads(self.rfile.read(int(self.headers.get('Content-Length', '0'))))
        session = self.headers.get('Mcp-Session-Id')
        headers = []
        if message.get('method') == 'initialize':
            session = uuid.uuid4().hex
            with LOCK:
                SESSIONS[session] = {}
            if os.environ.get('MCP_SESSIONS'):
                with open(os.environ['MCP_SESSIONS'], 'a') as f:
                    f.write(session + '\n')
            headers.append(('Mcp-Session-Id', session))
        elif session not in SESSIONS:
            self.reply(404, {'jsonrpc': '2.0', 'id': None,
                             'error': {'code': -32001, 'message': 'Session not found'}})
            return
        if 'id' not in message:
            self.reply(202, headers=headers)
            return
        self.reply(200, {'jsonrpc': '2.0', 'id': message['id'], 'result': answer(session, message)}, headers)


server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
print(server.server_address[1], flush=True)
sys.stdout.close()
server.serve_forever()
