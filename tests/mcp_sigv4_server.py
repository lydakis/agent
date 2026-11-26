"""A stand-in streamable HTTP MCP server behind AWS IAM, for Sonny's tests:
every request must carry an AWS Signature Version 4 that this server
checks as API Gateway does, over the method, path, query, the signed
headers and the SHA-256 of the body, for the service and region
$MCP_SIGV4 names (`execute-api/us-east-1` unset), under the secret
$MCP_KEYS (a JSON object of key id to secret) holds for its key id, with
`x-amz-security-token` signed when temporary keys sent one. A request
that fails is refused 403 with a JSON body, as API Gateway refuses one.

It offers one tool, `echo`, and `wait` (sleeps `ms`), and the MCP events
extension: `deploy.done`, polled, whose one event comes on the poll after
the first. Each request is appended to $MCP_SIGNED, when set, as a JSON
line: the HTTP method, the JSON-RPC method, the key id that signed it and
whether its signature held. It serves on a free port of 127.0.0.1 and
prints that port as its first line."""
import datetime
import hashlib
import hmac
import http.server
import json
import os
import sys
import threading
import time
import urllib.parse
import uuid

SERVICE, REGION = os.environ.get('MCP_SIGV4', 'execute-api/us-east-1').split('/')
KEYS = json.loads(os.environ.get('MCP_KEYS', '{}'))
EXTENSION = 'io.modelcontextprotocol/events'
TOOLS = [{'name': 'echo', 'description': 'Says it back.',
          'inputSchema': {'type': 'object', 'properties': {'text': {'type': 'string'}}},
          'annotations': {'readOnlyHint': True}},
         {'name': 'wait', 'description': 'Waits ms milliseconds.',
          'inputSchema': {'type': 'object', 'properties': {'ms': {'type': 'number'}}},
          'annotations': {'readOnlyHint': True}}]
OFFERED = [{'name': 'deploy.done', 'description': 'A deploy finished.', 'delivery': ['poll'],
            'inputSchema': {'type': 'object'}, 'payloadSchema': {'type': 'object'}}]
LOCK = threading.Lock()
SESSIONS = set()


def mac(key, text):
    return hmac.new(key, text.encode(), hashlib.sha256).digest()


def quote(text, safe):
    return urllib.parse.quote(text, safe=safe)


def verified(method, target, headers, body):
    """The key id that signed this request, and whether its signature held."""
    authorization = headers.get('Authorization', '')
    scheme, _, fields = authorization.partition(' ')
    fields = dict(part.strip().split('=', 1) for part in fields.split(',') if '=' in part)
    credential = fields.get('Credential', '').split('/')
    if scheme != 'AWS4-HMAC-SHA256' or len(credential) != 5:
        return None, False
    key, date, region, service, terminal = credential
    stamp = headers.get('X-Amz-Date', '')
    payload = headers.get('X-Amz-Content-Sha256', '')
    if (region, service, terminal) != (REGION, SERVICE, 'aws4_request') or not stamp.startswith(date):
        return key, False
    if payload != hashlib.sha256(body).hexdigest():
        return key, False
    signed = fields.get('SignedHeaders', '').split(';')
    if 'host' not in signed or 'x-amz-date' not in signed:
        return key, False
    if 'X-Amz-Security-Token' in headers and 'x-amz-security-token' not in signed:
        return key, False
    path, _, query = target.partition('?')
    pairs = sorted((quote(urllib.parse.unquote_plus(name), '-_.~'), quote(urllib.parse.unquote_plus(value), '-_.~'))
                   for name, _, value in (item.partition('=') for item in query.split('&') if item))
    canonical = '\n'.join([
        method,
        quote(path, '/-_.~') or '/',
        '&'.join(f'{name}={value}' for name, value in pairs),
        ''.join(f"{name}:{' '.join((headers.get(name) or '').split())}\n" for name in signed),
        ';'.join(signed),
        payload,
    ])
    scope = f'{date}/{REGION}/{SERVICE}/aws4_request'
    to_sign = '\n'.join(['AWS4-HMAC-SHA256', stamp, scope, hashlib.sha256(canonical.encode()).hexdigest()])
    secret = KEYS.get(key)
    if secret is None:
        return key, False
    signing = mac(mac(mac(mac(f'AWS4{secret}'.encode(), date), REGION), SERVICE), 'aws4_request')
    expected = hmac.new(signing, to_sign.encode(), hashlib.sha256).hexdigest()
    # Within the 5 minutes AWS allows a signature's time.
    at = datetime.datetime.strptime(stamp, '%Y%m%dT%H%M%SZ').replace(tzinfo=datetime.timezone.utc)
    fresh = abs((datetime.datetime.now(datetime.timezone.utc) - at).total_seconds()) < 300
    return key, fresh and hmac.compare_digest(expected, fields.get('Signature', ''))


def answer(message):
    method, params = message.get('method'), message.get('params') or {}
    if method == 'initialize':
        return {'protocolVersion': params.get('protocolVersion', '2025-06-18'),
                'capabilities': {'tools': {}, 'extensions': {EXTENSION: {}}},
                'serverInfo': {'name': 'ledger', 'version': '0'}}
    if method == 'tools/list':
        return {'tools': TOOLS}
    if method == 'tools/call':
        arguments = params.get('arguments') or {}
        if params.get('name') == 'wait':
            time.sleep(arguments.get('ms', 0) / 1000)
            return {'content': [{'type': 'text', 'text': 'waited'}]}
        return {'content': [{'type': 'text', 'text': arguments.get('text', '')}]}
    if method == 'events/list':
        return {'events': OFFERED}
    if method == 'events/poll':
        # No cursor starts from now; the poll after it hears the deploy.
        cursor = params.get('cursor')
        events = [] if cursor in (None, '1') else [
            {'eventId': 'd1', 'name': 'deploy.done', 'timestamp': '2026-11-26T08:00:00Z',
             'data': {'stage': 'prod'}}]
        return {'events': events, 'cursor': '0' if cursor is None else '1', 'truncated': False,
                'hasMore': False, 'nextPollMs': 50}
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

    def checked(self, body, message=None):
        key, good = verified(self.command, self.path, self.headers, body)
        if os.environ.get('MCP_SIGNED'):
            with LOCK, open(os.environ['MCP_SIGNED'], 'a') as f:
                f.write(json.dumps({'http': self.command, 'method': (message or {}).get('method'),
                                    'key': key, 'verified': good}) + '\n')
        if not good:
            self.reply(403, {'message': 'The request signature we calculated does not match the signature you provided.'},
                       [('x-amzn-ErrorType', 'InvalidSignatureException')])
        return good

    def do_GET(self):
        if self.checked(b''):
            self.reply(405)

    def do_DELETE(self):
        if self.checked(b''):
            with LOCK:
                SESSIONS.discard(self.headers.get('Mcp-Session-Id'))
            self.reply(200)

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get('Content-Length', '0')))
        message = json.loads(body)
        if not self.checked(body, message):
            return
        session, headers = self.headers.get('Mcp-Session-Id'), []
        if message.get('method') == 'initialize':
            session = uuid.uuid4().hex
            with LOCK:
                SESSIONS.add(session)
            headers.append(('Mcp-Session-Id', session))
        elif session not in SESSIONS:
            self.reply(404, {'jsonrpc': '2.0', 'id': None,
                             'error': {'code': -32001, 'message': 'Session not found'}})
            return
        if 'id' not in message:
            self.reply(202, headers=headers)
            return
        self.reply(200, {'jsonrpc': '2.0', 'id': message['id'], 'result': answer(message)}, headers)


server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
print(server.server_address[1], flush=True)
sys.stdout.close()
server.serve_forever()
