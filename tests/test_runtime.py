"""Actual Rust process, disk recovery, provider transport, and tool loop."""
import hashlib
import http.server
import json
import os
from pathlib import Path
import queue
import signal
import sqlite3
import subprocess
import tempfile
import threading
import time
import unittest

from bench.targets import clean_env
from bench.runtime_client import Client, serve_args


class Model(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def log_message(self, *_):
        pass

    def do_POST(self):
        try:
            request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
            self.server.requests.put(request)
            if hasattr(self.server, 'request_gates'):
                try:
                    gate = self.server.request_gates.get_nowait()
                except queue.Empty:
                    pass
                else:
                    gate.wait(timeout=5)
            if hasattr(self.server, 'routes'):
                # Sticky routing: a fresh token on every response; the client
                # should keep the first of its turn.
                self.server.routes.append(self.headers.get('x-codex-turn-state'))
            if hasattr(self.server, 'expected_authorization'):
                self.server.auth_checks.append(self.headers.get('Authorization') == self.server.expected_authorization)
            if getattr(self.server, 'reject_compaction', False) and request.get('instructions') == 'Summarize.':
                body = b'{"error":{"message":"synthetic compaction refusal"}}'
                self.send_response(400)
                self.send_header('Content-Length', str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                self.wfile.flush()
                return
            if request.get('instructions') == 'Summarize.' and getattr(self.server, 'compaction_refusals', 0):
                self.server.compaction_refusals -= 1
                body = b'{"error":{"message":"synthetic compaction rate limit"}}'
                self.send_response(429)
                self.send_header('Content-Length', str(len(body)))
                # Exhaust retries quickly, then force the ordinary call to park.
                self.send_header('Retry-After', '0.4' if self.server.compaction_refusals == 0 else '0.001')
                self.end_headers()
                self.wfile.write(body)
                self.wfile.flush()
                return
            assert self.path == '/v1/responses'
            assert request['model'] == 'synthetic-model'
            user = [i for i in request['input'] if i.get('role') == 'user'][-1]['content'][0]['text']
            attempts = getattr(self.server, 'attempts', {})
            attempt = attempts[user] = attempts.get(user, 0) + 1
            self.server.attempts = attempts
            if user == 'tool:park-rounds' and (attempt <= 8 or attempt == 10):
                body = b'{"error":{"message":"try later"}}'
                self.send_response(429 if attempt <= 8 else 503)
                self.send_header('Content-Type', 'application/json')
                self.send_header('Content-Length', str(len(body)))
                if attempt <= 8:
                    self.send_header('Retry-After', '0.3')
                self.end_headers()
                self.wfile.write(body)
                return
            if ((user.startswith(('flaky:', 'origin:', 'limited:', 'waitretry:')) and attempt == 1)
                    or user.startswith('limited-forever:') or (user == 'tool:limited' and attempt == 2)):
                # Transport-level refusals: a 503 or a CDN's 520 the next
                # attempt clears, a 429 with Retry-After (for tool:limited, on
                # the call after the tool), or a 429 that never lifts.
                body = json.dumps({'error': {'message': 'try later'}}).encode()
                self.send_response(520 if user.startswith('origin:') else
                                   503 if user.startswith(('flaky:', 'waitretry:')) else 429)
                self.send_header('Content-Type', 'application/json')
                self.send_header('Content-Length', str(len(body)))
                if not user.startswith(('flaky:', 'origin:', 'waitretry:')):
                    delays = getattr(self.server, 'retry_delays', ['0.05'])
                    self.send_header('Retry-After', delays[min(attempt - 1, len(delays) - 1)])
                self.end_headers()
                self.wfile.write(body)
                return
            if user == 'flaky:gate':
                self.server.gate_entered.set()
                self.server.release_headers.wait(timeout=5)
            if user == 'gate':
                self.server.release_headers.wait(timeout=5)
            if user == 'wait':
                time.sleep(5)
            if user == 'burst':
                time.sleep(.3)  # Allow the CLI to attach its live follower.
            if user == 'slow':
                time.sleep(.5)
            last = request['input'][-1]
            if user.startswith('waitgate:') and last.get('type') == 'function_call_output':
                self.server.wait_resumed.put(request['instructions'])
                self.server.release_waiters.wait(timeout=5)
            if user.startswith('budget:'):
                count = sum(i.get('type') == 'function_call' for i in request['input']) + 1
                text = '' if count <= 205 else 'done'
                output = ([{'type': 'function_call', 'name': 'wait', 'call_id': f'budget-{count}',
                            'arguments': json.dumps({'handles': [user[7:] if count == 100 else 'proc:999']})}]
                          if count <= 205 else [{'type': 'message', 'role': 'assistant',
                                                 'content': [{'type': 'output_text', 'text': text}]}])
            elif last.get('type') == 'function_call_output' and user.startswith('bgwait:') and '"handle"' in last['output']:
                # Second step of a start-then-wait turn: park on the process handle.
                text = ''
                output = [{'type': 'function_call', 'name': 'wait', 'call_id': 'wait-1',
                           'arguments': json.dumps({'handles': [json.loads(last['output'])['handle']]})}]
            elif user == 'cached:reused-call':
                text = ''
                output = [{'type': 'function_call', 'name': 'echo', 'call_id': 'same-id',
                           'arguments': '{"text":"ok"}'}]
            elif last.get('type') == 'function_call_output':
                text = 'echo:' + last['output']
                output = [{'id': 'msg_echo', 'type': 'message', 'role': 'assistant',
                           'content': [{'type': 'output_text', 'text': text}]}]
            elif user.startswith('bg:') or user.startswith('bgwait:'):
                text = ''
                command = user.split(':', 1)[1]
                output = [{'type': 'function_call', 'name': 'shell', 'call_id': 'bg-1',
                           'arguments': json.dumps({'command': command,
                               'timeout_ms': getattr(self.server, 'background_timeout_ms', 5000), 'background': True})}]
            elif user.startswith('note:') and last.get('type') != 'function_call_output':
                text = ''
                output = [{'type': 'function_call', 'name': 'note', 'call_id': 'note-1',
                           'arguments': json.dumps({'text': getattr(self.server, 'note_text', user[5:])})}]
            elif user.startswith('readart:'):
                text = ''
                reference, _, rest = user[8:].partition(' ')
                arguments = {'artifact': reference}
                if rest:
                    offset, limit = rest.split(',')
                    arguments.update(offset=int(offset), limit=int(limit))
                output = [{'type': 'function_call', 'name': 'read', 'call_id': 'readart-1',
                           'arguments': json.dumps(arguments)}]
            elif user.startswith('history:') and last.get('type') != 'function_call_output':
                text = ''
                output = [{'type': 'function_call', 'name': 'history', 'call_id': 'history-1',
                           'arguments': json.dumps({'turn': int(user[8:].split(',')[0]),
                                                    'offset': int(user.split(',')[1]) if ',' in user else 0,
                                                    **({'limit': int(user.split(',')[2])} if user.count(',') == 2 else {})})}]
                if getattr(self.server, 'history_prefill', 0):
                    text = 'p' * self.server.history_prefill
                    output.insert(0, {'type': 'message', 'role': 'assistant',
                                      'content': [{'type': 'output_text', 'text': text}]})
            elif user.startswith('waitany:'):
                text = ''
                output = [{'type': 'function_call', 'name': 'wait', 'call_id': 'wait-1',
                           'arguments': json.dumps({'handles': user[8:].split(','), 'any': True})}]
            elif user.startswith(('wait:', 'waitretry:')):
                text = ''
                output = [{'type': 'function_call', 'name': 'wait', 'call_id': 'wait-1',
                           'arguments': json.dumps({'handles': user.split(':', 1)[1].split(',')})}]
            elif user.startswith('waitgate:'):
                text = ''
                output = [{'type': 'function_call', 'name': 'wait', 'call_id': 'wait-1',
                           'arguments': json.dumps({'handles': [user[9:]]})}]
            elif user.startswith('waitthen:'):
                # One response with a wait followed by another call.
                text = ''
                output = [{'type': 'function_call', 'name': 'wait', 'call_id': 'wait-1',
                           'arguments': json.dumps({'handles': user[9:].split(',')})},
                          {'type': 'function_call', 'name': 'echo', 'call_id': 'after-1',
                           'arguments': json.dumps({'text': 'after-the-wait'})}]
            elif user.startswith('waitt:'):
                text = ''
                timeout, handles = user[6:].split(':', 1)
                output = [{'type': 'function_call', 'name': 'wait', 'call_id': 'wait-1',
                           'arguments': json.dumps({'handles': handles.split(','), 'timeout_ms': int(timeout)})}]
            elif user.startswith('detach:'):
                text = ''
                output = [{'type': 'function_call', 'name': 'shell', 'call_id': 'detach-1',
                           'arguments': json.dumps({'command': user[7:], 'detach': True})}]
            elif user.startswith('shell:'):
                text = ''
                output = [{'type': 'function_call', 'name': 'shell', 'call_id': 'shell-1',
                           'arguments': json.dumps({'command': user[6:], 'timeout_ms': 2000})}]
            elif user.startswith('tool:'):
                text = ''
                output = [{'type': 'function_call', 'name': 'echo', 'call_id': 'echo-1',
                           'arguments': json.dumps({'text': getattr(self.server, 'note_text', user[5:])})}]
            elif user == 'large-call-id':
                text = ''
                output = [{'type': 'function_call', 'name': 'echo', 'call_id': 'c' * 210000,
                           'arguments': json.dumps({'text': 'ok'})}]
            else:
                text = getattr(self.server, 'reply_text', 'reply:' + user)
                output = [{'id': 'msg_text', 'type': 'message', 'role': 'assistant',
                           'content': [{'type': 'output_text', 'text': text}]}]
            if request.get('instructions') == 'Summarize.':
                text = getattr(self.server, 'compaction_text', 'A short synthetic summary.')
                output = [{'type': 'message', 'role': 'assistant',
                           'content': [{'type': 'output_text', 'text': text}]}]
            if getattr(self.server, 'history_reasoning', None):
                output.insert(0, self.server.history_reasoning)
            if getattr(self.server, 'empty_compaction', False) and request.get('instructions') == 'Summarize.':
                text, output = '', []
            events = [{'type': 'response.created', 'response': {'id': 'response_test'}}]
            if text:
                events.append({'type': 'response.output_text.delta', 'delta': text})
            if user != 'truncate':
                # A dated snapshot answers, as providers name it.
                events.append({'type': 'response.completed', 'response': {
                    'model': request['model'] + '-2026-09-26', 'status': 'completed', 'output': output,
                    'usage': {'input_tokens': 100, 'output_tokens': 10,
                              'input_tokens_details': {'cached_tokens': 40 if user.startswith('cached:') else 0}}}})
            if user == 'incomplete':
                events = [{'type': 'response.incomplete', 'response': {'status': 'incomplete',
                    'incomplete_details': {'reason': 'max_output_tokens'}, 'output': [],
                    'usage': {'input_tokens': 100, 'output_tokens': 10}}}]
            if user.startswith('streamlimit:') and attempt == 1:
                # A rate limit the provider reports inside the stream.
                events = [{'type': 'response.failed', 'response': {'status': 'failed', 'error': {
                    'code': 'rate_limit_exceeded', 'message': 'Rate limit reached. Please try again in 300ms.'}}}]
            if user.startswith('streamlimit-flat:') and attempt == 1:
                events = [{'type': 'error', 'code': 'rate_limit_exceeded',
                           'message': 'Please try again in 300ms.', 'param': None, 'sequence_number': 1}]
            if user.startswith('tool:billed-retry:') and attempt == 1:
                events = [{'type': 'response.failed', 'response': {'status': 'failed',
                    'usage': {'input_tokens': 100, 'output_tokens': 10}, 'error': {
                    'code': 'rate_limit_exceeded', 'message': 'Please try again in 1ms.'}}}]
            self.send_response(200)
            self.send_header('Content-Type', 'text/event-stream')
            self.send_header('Transfer-Encoding', 'chunked')
            if hasattr(self.server, 'routes'):
                self.send_header('x-codex-turn-state', f'route-{len(self.server.routes)}')
            if user.startswith('paced:'):
                # The allowance is spent; the daemon must hold the next call.
                self.send_header('x-ratelimit-limit-tokens', '60000')
                self.send_header('x-ratelimit-remaining-tokens', '0')
                self.send_header('x-ratelimit-reset-tokens', '400ms')
            self.end_headers()
            if user == 'burst':
                text = 'reply:burst'
                deltas = [{'type': 'response.output_text.delta', 'delta': ''}] * 2000
                frames = deltas + events
                body = b''.join(('data: ' + json.dumps(e) + '\n\n').encode() for e in frames)
                self.wfile.write(f'{len(body):x}\r\n'.encode() + body + b'\r\n0\r\n\r\n')
                self.wfile.flush()
                return
            if user == 'gate':
                self.wfile.flush()
                self.server.all_streaming.wait(timeout=5)
            for event in events:
                encoded = json.dumps(event, ensure_ascii=False,
                                     indent=2 if getattr(self.server, 'history_multiline', False) else None)
                frame = (''.join('data: ' + line + '\r\n' for line in encoded.split('\n')) + '\r\n').encode()
                # Split inside UTF-8 sequences and SSE line boundaries.
                chunk_size = 8192 if user == 'large-call-id' or user.startswith('budget:') else 7
                for offset in range(0, len(frame), chunk_size):
                    part = frame[offset:offset + chunk_size]
                    self.wfile.write(f'{len(part):x}\r\n'.encode() + part + b'\r\n')
            self.wfile.write(b'0\r\n\r\n')
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass


def thinking_binding(request, before):
    """What a synthetic signature binds to, as newer Claude models do: the
    system prompt, the tool set, and every message before the block, minus
    earlier thinking blocks and cache markers."""
    def plain(value):
        if isinstance(value, dict):
            return {k: plain(v) for k, v in value.items() if k != 'cache_control'}
        if isinstance(value, list):
            return [plain(v) for v in value]
        return value
    messages = [{**m, 'content': [b for b in m['content'] if b['type'] not in ('thinking', 'redacted_thinking')]}
                for m in before]
    bound = [plain(request.get('system')), sorted(t['name'] for t in request.get('tools', [])), plain(messages)]
    return 'sig:' + hashlib.sha256(json.dumps(bound, sort_keys=True).encode()).hexdigest()[:16]


class AnthropicModel(http.server.BaseHTTPRequestHandler):
    """Synthetic Anthropic Messages endpoint: thinking, text, tool_use, tool_result.
    With `bind_thinking` set on the server, signatures bind to the conversation
    before them and a replayed block whose context changed is refused, as the
    strict check does; `report_drops` makes it report dropped blocks instead."""
    protocol_version = 'HTTP/1.1'

    def log_message(self, *_):
        pass

    def do_POST(self):
        try:
            request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
            self.server.requests.put(request)
            if hasattr(self.server, 'arrivals'):
                self.server.arrivals.append((time.monotonic(), request))
            assert self.path == '/v1/messages'
            assert self.headers.get('x-api-key') == 'synthetic-anthropic-key'
            assert self.headers.get('anthropic-version') == '2023-06-01'
            # Server-side fallbacks are a bot's choice: the field and its beta
            # header travel together, or not at all.
            if 'fallbacks' in request:
                assert request['fallbacks'] == 'default'
                assert self.headers.get('anthropic-beta') == (
                    'thinking-binding-controls-2026-08-01,server-side-fallback-2026-07-01')
            else:
                assert self.headers.get('anthropic-beta') == 'thinking-binding-controls-2026-08-01'
            if 'thinking' in request:
                assert request['thinking']['block_binding'] == {'prefix_mismatch_behavior': 'drop_block'}
            # A cache refresh is the same request with no output and no stream.
            warm = request['max_tokens'] == 0
            assert request['model'] == 'synthetic-claude' and request['stream'] != warm
            cache = getattr(self.server, 'cache_control', {'type': 'ephemeral'})
            for block in request.get('system', []):
                assert block['text'] and block['cache_control'] == cache
            assert request['cache_control'] == cache
            summary = request.get('system', [{}])[0].get('text') == 'Summarize.'
            history_uses_tools = any(b['type'] in ('tool_use', 'tool_result')
                                     for m in request['messages'] for b in m['content'])
            if (history_uses_tools and not request.get('tools')) or (
                    summary and request.get('tools') and request.get('tool_choice') != {'type': 'none'}):
                body = b'{"error":{"message":"tool history needs definitions; summarization must disable tool calls"}}'
                self.send_response(400)
                self.send_header('Content-Length', str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                self.wfile.flush()
                return
            if not summary:
                assert 'tool_choice' not in request
            assert [t['name'] for t in request['tools']] == ['echo', 'shell']
            assert 'input_schema' in request['tools'][0]
            last = request['messages'][-1]
            assert last['role'] == 'user'
            if warm and getattr(self.server, 'refuse_warm', False):
                body = b'{"type":"error","error":{"type":"invalid_request_error","message":"no"}}'
                self.send_response(400)
                self.send_header('Content-Length', str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                self.wfile.flush()
                return
            if warm:
                time.sleep(getattr(self.server, 'warm_delay', 0))
                body = json.dumps({'type': 'message', 'role': 'assistant', 'content': [],
                                   'model': request['model'] + '-20260926',
                                   'stop_reason': 'max_tokens', 'usage': {
                                       'input_tokens': 0, 'cache_read_input_tokens': 9,
                                       'cache_creation_input_tokens': 0, 'output_tokens': 0}}).encode()
                self.send_response(200)
                self.send_header('Content-Type', 'application/json')
                self.send_header('Content-Length', str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                self.wfile.flush()
                return
            signature = 'sig-1'
            if getattr(self.server, 'bind_thinking', False):
                for index, message in enumerate(request['messages']):
                    for block in message['content']:
                        if block['type'] == 'thinking' and block['signature'] != thinking_binding(
                                request, request['messages'][:index]):
                            self.server.binding_errors.append(index)
                            body = json.dumps({'type': 'error', 'error': {'type': 'invalid_request_error',
                                'message': f'messages.{index}: The block is bound to a different conversation.'}}).encode()
                            self.send_response(400)
                            self.send_header('Content-Length', str(len(body)))
                            self.end_headers()
                            self.wfile.write(body)
                            self.wfile.flush()
                            return
                signature = thinking_binding(request, request['messages'])
            blocks = [{'type': 'thinking', 'thinking': 'plan', 'signature': signature}]
            if last['content'][0]['type'] == 'tool_result':
                blocks.append({'type': 'text', 'text': 'echo:' + last['content'][0]['content']})
                stop = 'end_turn'
            else:
                user = last['content'][0]['text']
                if user == 'think-only':
                    stop = 'end_turn'
                elif user.startswith('tool:'):
                    blocks.append({'type': 'tool_use', 'id': 'toolu_1', 'name': 'echo', 'input': {'text': user[5:]}})
                    stop = 'tool_use'
                elif user.startswith('shell:'):
                    blocks.append({'type': 'tool_use', 'id': 'toolu_1', 'name': 'shell',
                                   'input': {'command': user[6:]}})
                    stop = 'tool_use'
                elif user.startswith('fallback:'):
                    # A classifier declines mid-output and another model
                    # finishes: the declined partial stays in the stream.
                    blocks += [{'type': 'text', 'text': 'Partial '},
                               {'type': 'tool_use', 'id': 'toolu_0', 'name': 'echo', 'input': {'text': 'declined'}},
                               {'type': 'fallback', 'from': {'model': 'synthetic-claude'},
                                'to': {'model': 'synthetic-fallback'}},
                               {'type': 'text', 'text': 'rest'},
                               {'type': 'tool_use', 'id': 'toolu_1', 'name': 'echo', 'input': {'text': user[9:]}}]
                    stop = 'tool_use'
                else:
                    blocks.append({'type': 'text', 'text': 'reply:' + user})
                    stop = 'max_tokens' if user == 'incomplete' else 'end_turn'
            start = {'model': request['model'] + '-20260926',
                     'usage': {'input_tokens': 5, 'cache_read_input_tokens': 2,
                               **getattr(self.server, 'start_usage', {})}}
            if getattr(self.server, 'report_drops', 0):
                start['input_transformations'] = [{'type': 'thinking_dropped', 'message_index': 1, 'block_index': 0}
                                                  for _ in range(self.server.report_drops)]
            events = [('message_start', {'message': start})]
            for index, block in enumerate(blocks):
                if block['type'] == 'fallback':
                    events.append(('content_block_start', {'index': index, 'content_block': block}))
                    continue
                start = {**block, 'thinking': ''} if block['type'] == 'thinking' else (
                    {**block, 'text': ''} if block['type'] == 'text' else {**block, 'input': {}})
                events.append(('content_block_start', {'index': index, 'content_block': start}))
                if block['type'] == 'thinking':
                    events.append(('content_block_delta', {'index': index, 'delta': {'type': 'thinking_delta', 'thinking': block['thinking']}}))
                    events.append(('content_block_delta', {'index': index, 'delta': {'type': 'signature_delta', 'signature': block['signature']}}))
                elif block['type'] == 'text':
                    for part in (block['text'][:3], block['text'][3:]):
                        events.append(('content_block_delta', {'index': index, 'delta': {'type': 'text_delta', 'text': part}}))
                else:
                    payload = json.dumps(block['input'])
                    for part in (payload[:4], payload[4:]):
                        events.append(('content_block_delta', {'index': index, 'delta': {'type': 'input_json_delta', 'partial_json': part}}))
                events.append(('content_block_stop', {'index': index}))
            usage = {'output_tokens': 7}
            if any(b['type'] == 'fallback' for b in blocks):
                usage['iterations'] = [
                    {'type': 'message', 'model': 'synthetic-claude', 'input_tokens': 5, 'cache_read_input_tokens': 2,
                     'cache_creation_input_tokens': 0, 'output_tokens': 4},
                    {'type': 'fallback_message', 'model': 'synthetic-fallback', 'input_tokens': 6,
                     'cache_read_input_tokens': 0, 'cache_creation_input_tokens': 1, 'output_tokens': 7}]
            events.append(('message_delta', {'delta': {'stop_reason': stop}, 'usage': usage}))
            events.append(('message_stop', {}))
            # A slow start: the answer to 'hold' waits this long for its headers.
            held = last['content'][0].get('text') == 'hold'
            if held:
                time.sleep(self.server.hold_delay)
            self.send_response(200)
            self.send_header('Content-Type', 'text/event-stream')
            self.send_header('Transfer-Encoding', 'chunked')
            self.end_headers()
            # A long reply: the first answer not held streams its start, then
            # pauses this long before the rest, as a long generation does.
            generating = 0 if held else getattr(self.server, 'generate_delay', 0)
            if not held:
                self.server.generate_delay = 0
            for index, (kind, body) in enumerate(events):
                frame = f'event: {kind}\ndata: {json.dumps({"type": kind, **body})}\n\n'.encode()
                self.wfile.write(f'{len(frame):x}\r\n'.encode() + frame + b'\r\n')
                if index == 0 and generating:
                    self.wfile.flush()
                    time.sleep(generating)
            self.wfile.write(b'0\r\n\r\n')
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class AnthropicRuntimeTests(unittest.TestCase):
    def start(self, extra=()):
        root = Path(__file__).resolve().parent.parent
        temp = tempfile.TemporaryDirectory(dir=root / '.local')
        self.addCleanup(temp.cleanup)
        path = Path(temp.name)
        model = http.server.ThreadingHTTPServer(('127.0.0.1', 0), AnthropicModel)
        model.requests = queue.Queue()
        model.daemon_threads = True
        threading.Thread(target=model.serve_forever, daemon=True).start()
        self.addCleanup(model.server_close)
        self.addCleanup(model.shutdown)
        env = {**clean_env(), 'ANTHROPIC_TEST_KEY': 'synthetic-anthropic-key'}
        client = Client(root / '.local/target/release/agent', path / 'state.sqlite',
                        f'http://127.0.0.1:{model.server_port}/v1', 'echo,shell', model='synthetic-claude',
                        key_env='ANTHROPIC_TEST_KEY', env=env, provider='anthropic', family='anthropic',
                        extra=extra)
        self.addCleanup(client.close)
        return client, model, path

    def test_a_long_tool_call_keeps_the_prompt_cache_warm(self):
        client, model, path = self.start(extra=('--keep-warm', '1'))
        client.request('create', bot='Bob', workspace=str(path), reasoning='low')
        turn = client.request('submit', bot='Bob', request_id='w1', prompt='shell:sleep 2.5')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        requests = []
        while not model.requests.empty():
            requests.append(model.requests.get())
        call, warms, answer = requests[0], requests[1:-1], requests[-1]
        # Refreshed once a second of the command's run, never after it ended;
        # each is the call's request with no output and no stream.
        self.assertEqual(len(warms), 2)
        for warm in warms:
            self.assertEqual(warm, {**call, 'max_tokens': 0, 'stream': False})
        self.assertEqual(answer['messages'][2]['content'][0]['type'], 'tool_result')
        # Each refresh is billed as the cache read it was, not as a model round,
        # and records when it was sent: a second apart, after the call.
        usage = [m['data'] for m in client.saved if m.get('event') == 'usage']
        sent = [u.pop('sent_ms') for u in usage]
        self.assertEqual([u for u in usage if u.get('purpose') == 'keep_warm'], [
            {'input_tokens': 9, 'output_tokens': 0, 'cached_input_tokens': 9, 'purpose': 'keep_warm',
             'served_model': 'synthetic-claude-20260926'}] * 2)
        self.assertEqual(len(usage), 4)
        self.assertEqual(sent, sorted(sent))
        self.assertGreaterEqual(sent[2] - sent[1], 900)

    def test_a_long_reply_keeps_its_own_prompt_cache_warm(self):
        client, model, path = self.start(extra=('--keep-warm', '1'))
        model.generate_delay = 2.5
        client.request('create', bot='Bob', workspace=str(path), reasoning='low')
        turn = client.request('submit', bot='Bob', request_id='g1', prompt='long')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        requests = []
        while not model.requests.empty():
            requests.append(model.requests.get())
        # Refreshed once a second while the reply streamed, never after it
        # ended; each is the call's own request with no output and no stream.
        call, warms = requests[0], requests[1:]
        self.assertEqual(len(warms), 2)
        for warm in warms:
            self.assertEqual(warm, {**call, 'max_tokens': 0, 'stream': False})
        usage = [m['data'] for m in client.saved if m.get('event') == 'usage']
        self.assertEqual([u.get('purpose') for u in usage], ['keep_warm', 'keep_warm', None])

    def test_a_refresh_in_flight_when_the_reply_ends_carries_into_the_tool(self):
        client, model, path = self.start(extra=('--keep-warm', '1'))
        model.generate_delay = 1.5
        model.warm_delay = 1
        client.request('create', bot='Bob', workspace=str(path), reasoning='low')
        turn = client.request('submit', bot='Bob', request_id='g2', prompt='shell:true')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        requests = []
        while not model.requests.empty():
            requests.append(model.requests.get())
        # The refresh sent a second into the reply is still unanswered when
        # the reply and then the tool end: it is answered and recorded once,
        # and nothing is sent after the tool.
        self.assertEqual([r['max_tokens'] == 0 for r in requests], [False, True, False])
        usage = [m['data'] for m in client.saved if m.get('event') == 'usage']
        self.assertEqual([u.get('purpose') for u in usage], [None, 'keep_warm', None])

    def test_a_steered_round_counts_the_last_replys_refresh_toward_the_budget(self):
        client, model, path = self.start(extra=('--keep-warm', '1'))
        model.generate_delay = 1.5
        model.warm_delay = 1
        # The call bills 14 tokens (5 + 2 cached in, 7 out) and its refresh 9.
        client.request('create', bot='Bob', workspace=str(path), reasoning='low', budget_tokens=20)
        turn = client.request('submit', bot='Bob', request_id='s1', prompt='long')['result']['turn']
        model.requests.get(timeout=5)
        client.request('submit', bot='Bob', request_id='s2', prompt='more', delivery='steer')
        # The refresh still in flight when the reply ends is answered and
        # counted before the steer's round, which the budget then refuses.
        self.assertEqual(client.finished(turn)['data']['error'], 'budget_exhausted')
        self.assertEqual(model.requests.get(timeout=1)['max_tokens'], 0)
        self.assertTrue(model.requests.empty())
        self.assertEqual(client.request('resume', bot='Bob')['result']['tokens_used'], 23)

    def test_a_call_waiting_to_be_sent_is_refreshed_only_from_its_send(self):
        # One request may start at a time and Ann's waits for its headers,
        # so Bob's call waits to be sent; its cache exists only from then.
        client, model, path = self.start(extra=('--keep-warm', '1', '--max-connecting', '1'))
        model.hold_delay = 2.5
        model.generate_delay = 1.5
        model.arrivals = []
        for bot in ('Ann', 'Bob'):
            client.request('create', bot=bot, workspace=str(path), reasoning='low')
        held = client.request('submit', bot='Ann', request_id='h1', prompt='hold')['result']['turn']
        model.requests.get(timeout=5)
        turn = client.request('submit', bot='Bob', request_id='h2', prompt='long')['result']['turn']
        for waited in (held, turn):
            self.assertEqual(client.finished(waited)['data']['status'], 'completed')
        bob = [(at, r) for at, r in model.arrivals if r['messages'][-1]['content'][0].get('text') == 'long']
        (sent, call), warms = bob[0], bob[1:]
        self.assertTrue(warms)
        for _, warm in warms:
            self.assertEqual(warm, {**call, 'max_tokens': 0, 'stream': False})
        self.assertGreaterEqual(warms[0][0] - sent, 0.9)

    def test_a_refused_refresh_ends_the_refreshes_but_not_the_turn(self):
        client, model, path = self.start(extra=('--keep-warm', '1'))
        model.refuse_warm = True
        client.request('create', bot='Bob', workspace=str(path), reasoning='low')
        turn = client.request('submit', bot='Bob', request_id='w1', prompt='shell:sleep 2.5')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        failed = [m for m in client.saved if m.get('event') == 'keep_warm_failed']
        self.assertEqual([(m['turn'], m['error'], m['durable']) for m in failed],
                         [(turn, 'provider_http_400', False)])
        warms = [r for r in iter(lambda: None if model.requests.empty() else model.requests.get(), None)
                 if r['max_tokens'] == 0]
        self.assertEqual(len(warms), 1)

    def test_a_refresh_sent_before_the_tool_ends_is_still_recorded(self):
        client, model, path = self.start(extra=('--keep-warm', '1'))
        model.warm_delay = 1
        client.request('create', bot='Bob', workspace=str(path), reasoning='low')
        turn = client.request('submit', bot='Bob', request_id='w1', prompt='shell:sleep 1.5')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        usage = [m['data'] for m in client.saved if m.get('event') == 'usage']
        self.assertEqual([u.get('purpose') for u in usage], [None, 'keep_warm', None])

    def test_an_interrupt_still_records_a_refresh_already_sent(self):
        client, model, path = self.start(extra=('--keep-warm', '1'))
        model.warm_delay = 1.5
        client.request('create', bot='Bob', workspace=str(path), reasoning='low')
        turn = client.request('submit', bot='Bob', request_id='i1', prompt='shell:sleep 10')['result']['turn']
        call = model.requests.get(timeout=5)
        self.assertEqual(model.requests.get(timeout=5)['max_tokens'], 0)  # the refresh is in flight
        client.request('interrupt', bot='Bob', turn=turn)
        self.assertEqual(client.finished(turn)['data']['status'], 'interrupted')
        usage = [m['data'] for m in client.saved if m.get('event') == 'usage']
        self.assertEqual([u.get('purpose') for u in usage], [None, 'keep_warm'])
        self.assertEqual(call['max_tokens'] > 0, True)

    def test_an_hour_long_cache_is_marked_priced_apart_and_not_refreshed(self):
        client, model, path = self.start(extra=('--keep-warm', '1', '--cache-ttl', '1h'))
        model.cache_control = {'type': 'ephemeral', 'ttl': '1h'}
        # A report without the per-lifetime split: every write is an hour's.
        model.start_usage = {'cache_creation_input_tokens': 3}
        client.request('create', bot='Bob', workspace=str(path), reasoning='low')
        turn = client.request('submit', bot='Bob', request_id='h1', prompt='shell:sleep 1.5')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertEqual([r['max_tokens'] > 0 for r in (model.requests.get(timeout=1),
                                                         model.requests.get(timeout=1))], [True, True])
        self.assertTrue(model.requests.empty())
        usage = [m['data'] for m in client.saved if m.get('event') == 'usage']
        self.assertEqual([(u['cache_write_tokens'], u['cache_write_1h_tokens']) for u in usage], [(3, 3)] * 2)

    def test_a_short_tool_call_sends_no_refresh(self):
        client, model, path = self.start()
        client.request('create', bot='Bob', workspace=str(path), reasoning='low')
        turn = client.request('submit', bot='Bob', request_id='w1', prompt='shell:true')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertEqual([r['max_tokens'] > 0 for r in (model.requests.get(timeout=1),
                                                         model.requests.get(timeout=1))], [True, True])
        self.assertTrue(model.requests.empty())

    def test_a_fallback_answer_replays_without_the_declined_attempt(self):
        client, model, path = self.start()
        client.request('create', bot='Bob', workspace=str(path), reasoning='low', fallbacks=True)
        turn = client.request('submit', bot='Bob', request_id='f1', prompt='fallback:kept')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        fallback = [m for m in client.saved if m.get('event') == 'model_fallback']
        self.assertEqual([(m['from'], m['to']) for m in fallback], [('synthetic-claude', 'synthetic-fallback')])
        # Both attempts produced output, so both are billed, each at its model,
        # and the answer the turn keeps came from the model fallen back to.
        usage = [m['data'] for m in client.saved if m.get('event') == 'usage']
        # Cache writes are recorded with the attempt that made them.
        usage[0].pop('sent_ms')
        self.assertEqual(usage[0], {'input_tokens': 14, 'output_tokens': 11, 'cached_input_tokens': 2,
                                    'cache_write_tokens': 1, 'served_model': 'synthetic-fallback', 'models': [
            {'model': 'synthetic-claude', 'input_tokens': 7, 'output_tokens': 4, 'cached_input_tokens': 2},
            {'model': 'synthetic-fallback', 'input_tokens': 7, 'output_tokens': 7, 'cached_input_tokens': 0,
             'cache_write_tokens': 1}]})
        model.requests.get(timeout=1)
        second = model.requests.get(timeout=1)
        # The declined attempt's text continues the answer; its thinking and
        # its tool call are neither replayed nor run. The marker stays in
        # place, since the API checks the thinking around it by position.
        self.assertEqual(second['messages'][1]['content'], [
            {'type': 'text', 'text': 'Partial '},
            {'type': 'fallback', 'from': {'model': 'synthetic-claude'}, 'to': {'model': 'synthetic-fallback'}},
            {'type': 'text', 'text': 'rest'},
            {'type': 'tool_use', 'id': 'toolu_1', 'name': 'echo', 'input': {'text': 'kept'}}])
        self.assertEqual(second['messages'][2]['content'],
                         [{'type': 'tool_result', 'tool_use_id': 'toolu_1', 'content': 'kept'}])

    def test_messages_family_round_trips_thinking_tools_and_usage(self):
        client, model, path = self.start()
        client.request('create', bot='Bob', workspace=str(path), reasoning='low', fallbacks=True)
        before = time.time() * 1000
        turn = client.request('submit', bot='Bob', request_id='r1', prompt='tool:shared')['result']['turn']
        finished = client.finished(turn)
        after = time.time() * 1000
        self.assertEqual(finished['data']['status'], 'completed')
        deltas = [m for m in client.saved if m.get('event') in ('text_delta', 'thinking_delta')]
        self.assertEqual([d['event'] for d in deltas][:2], ['thinking_delta', 'thinking_delta'])
        self.assertEqual(''.join(d['text'] for d in deltas if d['event'] == 'text_delta'), 'echo:shared')
        usage = [m for m in client.saved if m.get('event') == 'usage']
        self.assertEqual(len(usage), 2)
        # Each call records when it was sent, in epoch milliseconds.
        sent = [u['data'].pop('sent_ms') for u in usage]
        self.assertTrue(before - 1 <= sent[0] <= sent[1] <= after + 1, (before, sent, after))
        # The call names the dated model that answered, not only the one asked.
        self.assertEqual(usage[0]['data'], {'input_tokens': 7, 'output_tokens': 7, 'cached_input_tokens': 2,
                                            'served_model': 'synthetic-claude-20260926'})
        first, second = model.requests.get(timeout=1), model.requests.get(timeout=1)
        self.assertEqual(first['thinking'], {'type': 'adaptive', 'display': 'summarized',
                                             'block_binding': {'prefix_mismatch_behavior': 'drop_block'}})
        self.assertEqual(first['output_config'], {'effort': 'low'})
        self.assertEqual(first['messages'], [{'role': 'user', 'content': [{'type': 'text', 'text': 'tool:shared'}]}])
        assistant = second['messages'][1]
        self.assertEqual(assistant['role'], 'assistant')
        self.assertEqual(assistant['content'][0], {'type': 'thinking', 'thinking': 'plan', 'signature': 'sig-1'})
        self.assertEqual(assistant['content'][1]['input'], {'text': 'shared'})
        self.assertEqual(second['messages'][2]['content'][0],
                         {'type': 'tool_result', 'tool_use_id': 'toolu_1', 'content': 'shared'})
        self.assertEqual(len(second['messages']), 3)
        # The store is bound to the provider family; resume and replay are exact.
        state = client.request('resume', bot='Bob')['result']
        self.assertEqual((state['provider'], state['family'], state['model'], state['reasoning']),
                         ('anthropic', 'anthropic', 'synthetic-claude', 'low'))
        self.assertEqual(client.request('create', bot='Bad', workspace=str(path), reasoning='extreme')['error'],
                         'invalid_reasoning_level')
        self.assertEqual(client.request('create', bot='Bad', workspace=str(path), model='openai/x')['error'],
                         'provider_unavailable')
        client.request('create', bot='Capped', workspace=str(path), budget_tokens=10)
        capped = client.request('submit', bot='Capped', request_id='cap', prompt='incomplete')['result']['turn']
        self.assertEqual(client.finished(capped)['data']['error'], 'provider_incomplete')
        self.assertEqual(client.request('resume', bot='Capped')['result']['tokens_used'], 14)  # 5 + 2 cached in, 7 out
        self.assertEqual(client.request('submit', bot='Capped', request_id='retry', prompt='hello')['error'],
                         'budget_exhausted')
        while not model.requests.empty():
            model.requests.get_nowait()
        client.request('create', bot='Empty', workspace=str(path), instructions='')
        empty = client.request('submit', bot='Empty', request_id='e', prompt='hello')['result']['turn']
        self.assertEqual(client.finished(empty)['data']['status'], 'completed')
        self.assertNotIn('system', model.requests.get(timeout=1))


class ModelFixture(unittest.TestCase):
    handler = Model

    def setUp(self):
        root = Path(__file__).resolve().parent.parent
        self.temp = tempfile.TemporaryDirectory(dir=root / '.local')
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name)
        class Server(http.server.ThreadingHTTPServer):
            request_queue_size = 128
        self.model = Server(('127.0.0.1', 0), self.handler)
        self.model.requests = queue.Queue()
        self.model.daemon_threads = True
        self.worker = threading.Thread(target=self.model.serve_forever, daemon=True)
        self.worker.start()
        self.addCleanup(self.model.server_close)
        self.addCleanup(self.model.shutdown)
        self.binary = root / '.local/target/release/agent'
        self.url = f'http://127.0.0.1:{self.model.server_port}/v1'

    def client(self, tools="echo", extra=()):
        client = Client(self.binary, self.path / 'state.sqlite', self.url, tools, extra=extra)
        self.addCleanup(client.close)
        return client



@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class RuntimeTests(ModelFixture):
    def test_detached_shell_releases_its_daemon_slot_after_exit(self):
        client = self.client('shell', extra=('--max-detached', '1'))
        client.request('create', bot='Bob', workspace=str(self.path), tools=['shell'])

        def result(prompt, request_id):
            turn = client.request('submit', bot='Bob', request_id=request_id,
                                  prompt=prompt)['result']['turn']
            self.assertEqual(client.finished(turn)['data']['status'], 'completed')
            events = client.request('events', bot='Bob', after=0, limit=64)['result']['events']
            completed = next(e for e in events if e['turn'] == turn and e['event'] == 'tool_completed')
            output = client.request('item', bot='Bob', node=completed['data']['node'])['result']['output']
            return json.loads(output)

        first = result('detach:exec sleep 30', 'first')
        self.assertTrue(first['detached'])
        pid = first['pid']
        try:
            self.assertEqual(result('detach:true', 'full')['error'], 'detached_limit')
        finally:
            try:
                os.kill(pid, 9)
            except ProcessLookupError:
                pass
        for _ in range(100):
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                break
            time.sleep(.01)
        self.assertTrue(result('detach:true', 'free')['detached'])

    def test_shutdown_cancels_pending_retention_without_stdin_eof(self):
        client = self.client()
        client.request('create', bot='Big')
        client.request('shutdown')
        client.close()
        store = self.path / 'state.sqlite'
        # Synthetic cancelled queued turns need no transcript. Enough small
        # records to keep retention pending beyond shutdown's drain deadline,
        # without allocating large artifacts or making provider calls.
        with sqlite3.connect(store) as db:
            db.execute("""WITH RECURSIVE n(x) AS (
                VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1000000)
                INSERT INTO turns(id,bot,request_id,prompt,status)
                SELECT x,'Big',CAST(x AS TEXT),'p','cancelled' FROM n""")
            db.execute('INSERT INTO retained_turns SELECT id,bot FROM turns')
            db.execute('UPDATE turn_sequence SET last_id=1000000')
        for operation in ('prune', 'delete'):
            with self.subTest(operation=operation):
                client = self.client()
                try:
                    client.next_id += 1
                    request = {'id': client.next_id, 'op': operation, 'bot': 'Big'}
                    if operation == 'prune':
                        request['keep_turns'] = 1
                    client.process.stdin.write(json.dumps(request) + '\n')
                    client.process.stdin.flush()
                    # Ensure at least one piece committed before asking to stop.
                    label = 'delete_bot' if operation == 'delete' else 'prune'
                    deadline = time.monotonic() + 5
                    while True:
                        ops = client.request('stats')['result']['store']['operations']
                        if ops.get(label, {}).get('count', 0):
                            break
                        self.assertLess(time.monotonic(), deadline)
                    self.assertTrue(client.request('shutdown')['result']['shutting_down'])
                    self.assertFalse(client.process.stdin.closed)
                    self.assertEqual(client.process.wait(timeout=3), 0)
                    with sqlite3.connect(store) as db:
                        self.assertGreater(db.execute('SELECT count(*) FROM retained_turns').fetchone()[0], 0)
                        status = db.execute("SELECT status FROM bots WHERE name='Big'").fetchone()[0]
                        self.assertEqual(status, 'deleting' if operation == 'delete' else 'idle')
                finally:
                    client.close(kill=True)

    def test_stdio_shutdown_with_background_work_releases_publisher_and_store(self):
        self.model.background_timeout_ms = 30000
        client = self.client('shell')
        self.addCleanup(lambda: client.close(kill=True))
        client.request('create', bot='Bob', workspace=str(self.path))
        turn = client.request('submit', bot='Bob', request_id='background',
                              prompt='bg:sleep 30')['result']['turn']
        terminal = client.finished(turn)
        self.assertEqual(terminal['data']['status'], 'completed')
        self.assertEqual(client.request('stats')['result']['running_processes'], 1)
        # The background result task still owns a Store clone. Keep stdin
        # open and stdout draining: shutdown must release its publisher even
        # though that clone keeps the publication channel open.
        self.assertTrue(client.request('shutdown')['result']['shutting_down'])
        self.assertEqual(client.process.wait(timeout=8), 0)
        restarted = self.client('shell')
        replay = restarted.request('events', bot='Bob', after=0, limit=256)['result']['events']
        self.assertEqual(next(e for e in replay if e['event'] == 'turn_finished')['data'], terminal['data'])
        self.assertEqual(restarted.request('stats')['result']['running_processes'], 0)

    def test_stdio_shutdown_exits_and_releases_store_without_stdin_eof(self):
        client = self.client()
        self.addCleanup(lambda: client.close(kill=True))
        client.request('create', bot='Bob', workspace=str(self.path))
        turn = client.request('submit', bot='Bob', request_id='shutdown', prompt='wait')['result']['turn']
        self.model.requests.get(timeout=3)
        handle = f'turn:Bob/{turn}'
        client.process.stdin.write(json.dumps({'id': 'waiter', 'op': 'wait', 'handles': [handle]}) + '\n')
        client.process.stdin.flush()
        self.assertTrue(client.request('shutdown')['result']['shutting_down'])
        terminal = client.finished(turn)
        self.assertEqual((terminal['data']['status'], terminal['data']['error']), ('interrupted', 'daemon_shutdown'))
        waited = client.receive(lambda e: e.get('id') == 'waiter')['result']
        self.assertEqual(waited['results'][handle]['error'], 'daemon_shutdown')
        self.assertEqual(waited['pending'], [])
        self.assertEqual(client.process.wait(timeout=2), 0)
        restarted = self.client()
        replay = restarted.request('events', bot='Bob', after=0, limit=256)['result']['events']
        self.assertEqual(next(e for e in replay if e['event'] == 'turn_finished')['data'], terminal['data'])

    def test_shutdown_grace_lets_running_turns_finish_and_holds_new_ones(self):
        self.model.release_headers = threading.Event()
        self.model.all_streaming = self.model.release_headers
        client = self.client()
        self.addCleanup(lambda: client.close(kill=True))
        client.request('create', bot='Bob', workspace=str(self.path))
        client.request('create', bot='Alice', workspace=str(self.path))
        turn = client.request('submit', bot='Bob', request_id='running', prompt='gate')['result']['turn']
        self.model.requests.get(timeout=3)
        self.assertEqual(client.request('shutdown', grace_ms=86_400_001)['error'], 'invalid_timeout')
        self.assertFalse(client.request('stats')['result']['draining'])
        self.assertTrue(client.request('shutdown', grace_ms=5000)['result']['shutting_down'])
        self.assertTrue(client.request('stats')['result']['draining'])
        # Nothing new starts while draining: work that would start now is
        # refused, and queued work waits durably for the next start.
        refused = client.request('submit', bot='Alice', request_id='refused', prompt='hello')
        self.assertEqual(refused['error'], 'daemon_draining')
        held = client.request('submit', bot='Alice', request_id='held', prompt='hello', delivery='queue')['result']
        self.assertEqual(held['status'], 'ready')
        self.model.release_headers.set()
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertEqual(client.process.wait(timeout=3), 0)
        self.assertTrue(self.model.requests.empty())
        # The next start runs what the drain held.
        restarted = self.client()
        handle = f"turn:Alice/{held['turn']}"
        waited = restarted.request('wait', handles=[handle], timeout_ms=5000)['result']
        self.assertEqual(waited['results'][handle]['status'], 'completed')

    def test_shutdown_grace_expiry_cancels_with_the_shutdown_cause(self):
        client = self.client()
        self.addCleanup(lambda: client.close(kill=True))
        client.request('create', bot='Bob', workspace=str(self.path))
        turn = client.request('submit', bot='Bob', request_id='running', prompt='wait')['result']['turn']
        self.model.requests.get(timeout=3)
        self.assertTrue(client.request('shutdown', grace_ms=60_000)['result']['shutting_down'])
        # A later shutdown can only bring the deadline closer.
        started = time.monotonic()
        self.assertTrue(client.request('shutdown', grace_ms=300)['result']['shutting_down'])
        terminal = client.finished(turn)
        self.assertEqual((terminal['data']['status'], terminal['data']['error']), ('interrupted', 'daemon_shutdown'))
        self.assertGreaterEqual(time.monotonic() - started, .25)
        self.assertEqual(client.process.wait(timeout=3), 0)
        restarted = self.client()
        result = restarted.request('result', bot='Bob', turn=turn)['result']
        self.assertEqual((result['status'], result['error']), ('interrupted', 'daemon_shutdown'))

    def held_admissions(self, client, count):
        """Submissions to `count` bots, each of which would start a turn that
        stays on the model until shutdown ends it. The first one's store job
        takes over a second, so the rest queue behind it uncommitted."""
        bots = ['Slow'] + [f'B{index}' for index in range(count)]
        for bot in bots:
            client.request('create', bot=bot, workspace=str(self.path))
        with sqlite3.connect(self.path / 'state.sqlite') as db:
            db.execute('CREATE TABLE burn(x)')
            db.executemany('INSERT INTO burn VALUES (?)', [(n,) for n in range(6000)])
            db.execute("CREATE TRIGGER slow_admission AFTER INSERT ON turns WHEN NEW.bot='Slow' "
                       "BEGIN SELECT count(*) FROM burn a, burn b WHERE a.x+b.x>=0; END")
        return [{'id': f'submit-{bot}', 'op': 'submit', 'bot': bot, 'request_id': 'held', 'prompt': 'wait'}
                for bot in bots]

    def answered(self, client):
        return any('id' in message for message in [*client.saved, *list(client.queue.queue)] if message)

    def assert_shutdown_ended(self, client, turns):
        """Shutdown ended every started turn with its own cause, rather than
        leaving it running for the next start to find."""
        for turn in turns:
            data = client.finished(turn)['data']
            self.assertEqual((data['status'], data['error']), ('interrupted', 'daemon_shutdown'))
        self.assertEqual(client.process.wait(timeout=10), 0)
        with sqlite3.connect(self.path / 'state.sqlite') as db:
            self.assertEqual(db.execute("SELECT count(*) FROM turns WHERE status!='interrupted'").fetchone()[0], 0)
            self.assertEqual(db.execute("SELECT count(*) FROM bots WHERE running_turn IS NOT NULL").fetchone()[0], 0)

    def test_a_shutdown_request_behind_queued_admissions_waits_for_their_answers(self):
        client = self.client()
        self.addCleanup(lambda: client.close(kill=True))
        lines = self.held_admissions(client, 8) + [{'id': 'stop', 'op': 'shutdown'}]
        client.process.stdin.write(''.join(json.dumps(line) + '\n' for line in lines))
        client.process.stdin.flush()
        # The shutdown is answered after every admission queued ahead of it,
        # and each admission's turn started before shutdown ended it.
        replies = [client.receive(lambda m: 'id' in m) for _ in lines]
        self.assertEqual([r['id'] for r in replies], [line['id'] for line in lines])
        self.assertTrue(replies[-1]['result']['shutting_down'])
        self.assertEqual({r['result']['status'] for r in replies[:-1]}, {'running'})
        self.assert_shutdown_ended(client, [r['result']['turn'] for r in replies[:-1]])

    def test_a_termination_signal_still_answers_the_admissions_it_finds_queued(self):
        client = self.client()
        self.addCleanup(lambda: client.close(kill=True))
        lines = self.held_admissions(client, 8)
        client.process.stdin.write(''.join(json.dumps(line) + '\n' for line in lines))
        client.process.stdin.flush()
        time.sleep(.2)
        # The signal lands while every admission still waits on its commit.
        self.assertFalse(self.answered(client))
        client.process.send_signal(signal.SIGTERM)
        turns = []
        for line in lines:
            reply = client.receive(lambda m, id=line['id']: m.get('id') == id)
            self.assertEqual(reply['result']['status'], 'running')
            turns.append(reply['result']['turn'])
        self.assert_shutdown_ended(client, turns)

    def test_request_startup_is_bounded_but_established_streams_are_not(self):
        self.model.release_headers = threading.Event()
        self.model.all_streaming = threading.Barrier(70)
        self.addCleanup(self.model.release_headers.set)
        # The bound is off by default; this test asks for it explicitly.
        client = self.client(extra=('--max-connecting', '64'))
        turns = []
        for index in range(70):
            bot = str(index)
            client.request('create', bot=bot, workspace=str(self.path))
            turns.append(client.request('submit', bot=bot, request_id='gate', prompt='gate')['result']['turn'])
        for _ in range(64):
            self.model.requests.get(timeout=3)
        with self.assertRaises(queue.Empty):
            self.model.requests.get(timeout=.1)
        self.model.release_headers.set()
        for turn in turns:
            self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        # All 70 responses had to send headers before any could complete its body.
        self.assertEqual(self.model.all_streaming.n_waiting, 0)

    def test_kill_restart_resume_historical_fork_and_tool_loop(self):
        client = self.client()
        self.assertEqual(client.request('resume', bot='missing')['error'], 'bot_not_found')
        client.request('create', bot='Bob', workspace=str(self.path))
        first = client.request('submit', bot='Bob', request_id='first', prompt='héllo')['result']['turn']
        checkpoint = client.finished(first)['data']['checkpoint']
        second = client.request('submit', bot='Bob', request_id='second', prompt='second')['result']['turn']
        client.finished(second)
        before = client.request('events', bot='Bob', after=0, limit=256)['result']
        client.close(kill=True)
        client = self.client()
        self.assertEqual(client.request('resume', bot='Bob')['result']['status'], 'completed')
        self.assertEqual(client.request('events', bot='Bob', after=0, limit=256)['result'], before)
        self.assertTrue(client.request('submit', bot='Bob', request_id='first', prompt='héllo')['result']['duplicate'])
        client.request('fork', source='Bob', checkpoint=checkpoint, bot='Alternative', workspace=str(self.path))
        alt = client.request('submit', bot='Alternative', request_id='alt', prompt='tool:shared prefix')['result']['turn']
        self.assertEqual(client.finished(alt)['data']['status'], 'completed')
        requests = []
        while not self.model.requests.empty():
            requests.append(self.model.requests.get())
        self.assertEqual(len(requests), 4)
        alt_history = requests[2]['input']
        self.assertEqual(alt_history[0]['content'][0]['text'], 'héllo')
        self.assertEqual(alt_history[1]['content'][0]['text'], 'reply:héllo')
        self.assertNotIn('second', json.dumps(alt_history))
        self.assertEqual(requests[3]['input'][-1]['output'], 'shared prefix')
        self.assertEqual(client.request('resume', bot='Bob')['result']['head'], before['events'][-1]['data']['checkpoint'])

    def test_a_turn_keeps_its_first_routing_token_and_the_next_turn_starts_without_one(self):
        self.model.routes = []
        client = self.client()
        client.request('create', bot='Bob', workspace=str(self.path))
        for request_id, prompt in (('a', 'tool:one'), ('b', 'tool:two')):
            turn = client.request('submit', bot='Bob', request_id=request_id, prompt=prompt)['result']['turn']
            self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        # Each turn is two calls; the second carries the token the first got back.
        self.assertEqual(self.model.routes, [None, 'route-1', None, 'route-3'])

    def test_a_turn_keeps_its_routing_token_across_a_wait_and_a_rate_limit_park(self):
        self.model.routes = []
        self.model.retry_delays = ['0.3']
        client = self.client('echo,shell,wait')
        client.request('create', bot='Bob', workspace=str(self.path))
        # Start a process, then park on it until it exits.
        turn = client.request('submit', bot='Bob', request_id='w', prompt='bgwait:sleep .2')['result']['turn']
        client.receive(lambda m: m.get('event') == 'turn_waiting' and m.get('turn') == turn)
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        # The call after the tool is refused for pace and the turn parks.
        turn = client.request('submit', bot='Bob', request_id='p', prompt='tool:limited')['result']['turn']
        client.receive(lambda m: m.get('event') == 'turn_paced' and m.get('turn') == turn)
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        # Each resumed call still carries its turn's first token.
        self.assertEqual(self.model.routes, [None, 'route-1', 'route-1', None, 'route-4', 'route-4'])

    def test_each_calls_usage_records_when_it_was_sent(self):
        client = self.client()
        client.request('create', bot='Bob', workspace=str(self.path))
        before = time.time() * 1000
        turn = client.request('submit', bot='Bob', request_id='s', prompt='tool:one')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        after = time.time() * 1000
        # Two calls, each placed in time, so a cache miss can be set against
        # the gap since the call before it.
        sent = [m['data']['sent_ms'] for m in client.saved if m.get('event') == 'usage']
        self.assertEqual(len(sent), 2)
        self.assertTrue(before - 1 <= sent[0] <= sent[1] <= after + 1, (before, sent, after))
        stored = client.request('events', bot='Bob', after=0, limit=256)['result']['events']
        self.assertEqual([e['data']['sent_ms'] for e in stored if e['event'] == 'usage'], sent)

    def test_tools_are_per_bot_shown_to_the_model_and_enforced_at_dispatch(self):
        client = self.client('echo,shell')
        self.assertEqual(client.request('create', bot='Bad', workspace=str(self.path), tools=['echo', 'sudo'])['error'],
                         'unsupported_tool_set')
        self.assertEqual(client.request('create', bot='Twice', workspace=str(self.path), tools=['echo', 'echo'])['error'],
                         'unsupported_tool_set')
        client.next_id += 1
        client.process.stdin.write(json.dumps({'id': client.next_id, 'op': 'create', 'bot': 'None', 'workspace': str(self.path),
                                               'model': 'openai/synthetic-model', 'instructions': 'x'}) + '\n')
        client.process.stdin.flush()
        self.assertEqual(client.receive(lambda m: m.get('id') == client.next_id)['error'], 'tools_required')
        echo_only = client.request('create', bot='Echo', workspace=str(self.path), tools=['echo'])['result']
        self.assertEqual(echo_only['tools'], ['echo'])
        both = client.request('create', bot='Both', workspace=str(self.path), tools=['shell', 'echo'])['result']
        self.assertEqual(both['tools'], ['shell', 'echo'])
        # Each bot's request carries its own tools, and the daemon announces the universe.
        turn = client.request('submit', bot='Echo', request_id='1', prompt='shell:true')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        first = self.model.requests.get(timeout=3)
        self.assertEqual([t['name'] for t in first['tools']], ['echo'])
        events = client.request('events', bot='Echo', after=0, limit=64)['result']['events']
        done = [e for e in events if e['event'] == 'tool_completed'][0]
        output = client.request('item', bot='Echo', node=done['data']['node'])['result']['output']
        self.assertEqual(json.loads(output)['error'], 'tool_not_available')
        turn = client.request('submit', bot='Both', request_id='1', prompt='shell:true')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.model.requests.get(timeout=3)
        second = self.model.requests.get(timeout=3)
        self.assertEqual([t['name'] for t in second['tools']], ['shell', 'echo'])
        # A fork keeps its source's selection; a restart with other ideas changes nothing.
        fork = client.request('fork', source='Echo', bot='Branch')['result']
        self.assertEqual(fork['tools'], ['echo'])
        client.close()
        client = self.client('shell')
        self.assertEqual(client.request('resume', bot='Both')['result']['tools'], ['shell', 'echo'])
        self.assertEqual(client.request('stats')['result']['active_turns'], 0)

    def test_a_bot_is_created_with_what_its_client_states_and_keeps_it(self):
        client = self.client()
        raw = lambda **params: client.request('create', bot='Bob', workspace=str(self.path), **params)
        client.next_id += 1
        client.process.stdin.write(json.dumps({'id': client.next_id, 'op': 'create', 'bot': 'Bob',
                                               'workspace': str(self.path), 'instructions': 'x'}) + '\n')
        client.process.stdin.flush()
        self.assertEqual(client.receive(lambda m: m.get('id') == client.next_id)['error'], 'model_required')
        client.next_id += 1
        client.process.stdin.write(json.dumps({'id': client.next_id, 'op': 'create', 'bot': 'Bob',
                                               'workspace': str(self.path), 'model': 'openai/synthetic-model'}) + '\n')
        client.process.stdin.flush()
        self.assertEqual(client.receive(lambda m: m.get('id') == client.next_id)['error'], 'instructions_required')
        created = raw(model='openai/synthetic-model', instructions='Be brief.')['result']
        self.assertEqual((created['model'], created['instructions']), ('synthetic-model', 'Be brief.'))
        turn = client.request('submit', bot='Bob', request_id='1', prompt='hello')['result']['turn']
        client.finished(turn)
        self.assertEqual(self.model.requests.get(timeout=3)['instructions'], 'Be brief.')
        # Another client with other ideas changes nothing about Bob.
        client.close()
        client = Client(self.binary, self.path / 'state.sqlite', self.url, model='other-model')
        self.addCleanup(client.close)
        self.assertEqual(client.request('resume', bot='Bob')['result']['instructions'], 'Be brief.')

    def test_stores_open_under_any_provider_set_and_turns_check_the_family(self):
        client = self.client()
        client.request('create', bot='Bob', workspace=str(self.path))
        first = client.request('submit', bot='Bob', request_id='1', prompt='hello')['result']['turn']
        self.assertEqual(client.finished(first)['data']['status'], 'completed')
        before = client.request('resume', bot='Bob')['result']
        client.close()
        # A daemon without Bob's provider opens the store; Bob's turns fail by name.
        client = Client(self.binary, self.path / 'state.sqlite', self.url, provider='other')
        self.addCleanup(client.close)
        client.request('create', bot='Alice', workspace=str(self.path), model='other/synthetic-model')
        alice = client.request('submit', bot='Alice', request_id='a', prompt='hi')['result']['turn']
        self.assertEqual(client.finished(alice)['data']['status'], 'completed')
        for delivery in ('reject', 'queue', 'steer'):
            missing = client.request('submit', bot='Bob', request_id='2', prompt='again', delivery=delivery)
            self.assertEqual(missing['error'], 'provider_unavailable')
        self.assertEqual(client.request('resume', bot='Bob')['result'], before)
        duplicate = client.request('submit', bot='Bob', request_id='1', prompt='hello')['result']
        self.assertTrue(duplicate['duplicate'])
        self.assertEqual(duplicate['turn'], first)
        client.close()
        # The same name with another encoding is refused; the transcript is untouched.
        client = Client(self.binary, self.path / 'state.sqlite', self.url, family='anthropic')
        self.addCleanup(client.close)
        for model in (None, 'openai/synthetic-model'):
            wrong = client.request('submit', bot='Bob', request_id='3', prompt='again', model=model)
            self.assertEqual(wrong['error'], 'provider_family_mismatch')
        self.assertEqual(client.request('resume', bot='Bob')['result'], before)
        client.close()
        # Back with the provider, the conversation continues where it was.
        client = self.client()
        later = client.request('submit', bot='Bob', request_id='2', prompt='back')['result']['turn']
        self.assertEqual(client.finished(later)['data']['status'], 'completed')
        self.assertEqual(client.request('result', bot='Bob', turn=later)['result']['text'], 'reply:back')

    def test_fork_from_a_mid_turn_message_and_from_the_head(self):
        client = self.client('echo,shell')
        client.request('create', bot='Bob', workspace=str(self.path))
        turn = client.request('submit', bot='Bob', request_id='t', prompt='tool:shared')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        events = client.request('events', bot='Bob', after=0, limit=256)['result']['events']
        messages = [e['data']['node'] for e in events if e['event'] == 'message']
        answered = next(e['data']['node'] for e in events if e['event'] == 'tool_completed')
        # The assistant item that planned the call is unanswered at that point.
        self.assertEqual(client.request('fork', source='Bob', checkpoint=messages[0], bot='early',
                                        workspace=str(self.path))['error'], 'fork_point_has_open_tool_calls')
        branch = client.request('fork', source='Bob', checkpoint=answered, bot='branch', workspace=str(self.path))['result']
        self.assertEqual(branch['head'], answered)
        tip = client.request('fork', source='Bob', bot='tip', workspace=str(self.path))['result']
        self.assertEqual(tip['head'], client.request('resume', bot='Bob')['result']['head'])
        # The branch continues from the tool result; the provider sees exactly that prefix.
        t2 = client.request('submit', bot='branch', request_id='b', prompt='after the tool')['result']['turn']
        self.assertEqual(client.finished(t2)['data']['status'], 'completed')
        while not self.model.requests.empty():
            request = self.model.requests.get()
        self.assertEqual(request['input'][-2]['type'], 'function_call_output')
        self.assertEqual(request['input'][-1]['content'][0]['text'], 'after the tool')
        self.assertEqual(len(request['input']), 4)
        self.assertEqual(client.request('fork', source='Bob', checkpoint=99999, bot='nope',
                                        workspace=str(self.path))['error'], 'node_not_in_source_history')
        # A fork is an exact copy of its source: it takes no instructions.
        client.process.stdin.write(json.dumps({'id': 'copy', 'op': 'fork', 'source': 'Bob', 'bot': 'other',
                                               'instructions': 'Other.'}) + '\n')
        client.process.stdin.flush()
        self.assertEqual(client.receive(lambda m: 'error' in m and m.get('id') is None)['error'], 'invalid_json')
        self.assertEqual(client.request('resume', bot='other')['error'], 'bot_not_found')

    def test_turn_overrides_workspace_and_model_within_the_family(self):
        client = self.client('echo,shell')
        client.request('create', bot='Bob', workspace=str(self.path))
        other = self.path / 'other'
        other.mkdir()
        turn = client.request('submit', bot='Bob', request_id='w', prompt='shell:printf x > marker',
                              workspace=str(other), model='openai/synthetic-model')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertTrue((other / 'marker').exists())
        self.assertFalse((self.path / 'marker').exists())
        self.assertEqual(client.request('submit', bot='Bob', request_id='w', prompt='shell:printf x > marker',
                                        workspace=str(self.path))['error'], 'idempotency_conflict')
        self.assertEqual(client.request('submit', bot='Bob', request_id='m', prompt='hi',
                                        model='anthropic/claude')['error'], 'provider_unavailable')
        self.assertEqual(client.request('submit', bot='Bob', request_id='m', prompt='hi',
                                        workspace='relative/path')['error'], 'workspace_must_exist_and_be_absolute')
        self.assertEqual(client.request('resume', bot='Bob')['result']['workspace'], str(self.path.resolve()))
        # A bot created without a workspace must be given one per submission.
        self.assertIsNone(client.request('create', bot='Nomad')['result']['workspace'])
        self.assertEqual(client.request('submit', bot='Nomad', request_id='n', prompt='hi')['error'], 'workspace_required')
        turn = client.request('submit', bot='Nomad', request_id='n2', prompt='hi', workspace=str(other))['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')

    def test_interrupt_and_crash_recovery_are_explicit(self):
        client = self.client()
        client.request('create', bot='Bob', workspace=str(self.path))
        turn = client.request('submit', bot='Bob', request_id='wait-1', prompt='wait')['result']['turn']
        self.model.requests.get(timeout=3)
        self.assertEqual(client.request('interrupt', bot='Bob', turn=turn+1)['error'], 'stale_turn')
        client.request('interrupt', bot='Bob', turn=turn)
        self.assertEqual(client.finished(turn)['data']['status'], 'interrupted')
        pending = client.request('submit', bot='Bob', request_id='wait-2', prompt='wait')['result']['turn']
        self.model.requests.get(timeout=3)
        client.close(kill=True)
        client = self.client()
        self.assertEqual(client.request('resume', bot='Bob')['result']['status'], 'interrupted')
        events = client.request('events', bot='Bob', after=0, limit=256)['result']['events']
        self.assertTrue(any(e.get('turn') == pending and e['data'].get('error') == 'process_interrupted' for e in events))
        self.assertTrue(self.model.requests.empty())  # Restart did not launch a paid/repeated request.

    def test_crash_during_a_tool_keeps_the_same_bot_usable(self):
        client = self.client('echo,shell')
        client.request('create', bot='Bob', workspace=str(self.path))
        turn = client.request('submit', bot='Bob', request_id='crash',
                              prompt='shell:echo started > crash-marker; sleep 1')['result']['turn']
        deadline = time.monotonic() + 3
        while not (self.path / 'crash-marker').exists() and time.monotonic() < deadline:
            time.sleep(.01)
        self.assertTrue((self.path / 'crash-marker').exists())
        client.close(kill=True)
        self.model.requests.get(timeout=1)
        client = self.client('echo,shell')
        self.assertEqual(client.request('resume', bot='Bob')['result']['status'], 'interrupted')
        events = client.request('events', bot='Bob', after=0, limit=256)['result']['events']
        results = [e for e in events if e['event'] == 'tool_completed' and e['turn'] == turn]
        self.assertEqual(len(results), 1)
        self.assertTrue(results[0]['data']['outcome_unknown'])
        self.assertTrue(self.model.requests.empty())  # Recovery never replays the tool.
        again = client.request('submit', bot='Bob', request_id='continue', prompt='hi')['result']['turn']
        self.assertEqual(client.finished(again)['data']['status'], 'completed')
        history = self.model.requests.get(timeout=1)['input']
        call = next(i for i in history if i.get('type') == 'function_call')
        result = next(i for i in history if i.get('type') == 'function_call_output')
        self.assertEqual(result['call_id'], call['call_id'])
        self.assertEqual(json.loads(result['output'])['error'], 'tool_outcome_unknown')

    def test_shell_workspace_result_and_cancelled_descendants(self):
        import psutil
        client = self.client('echo,shell')
        client.request('create', bot='Bob', workspace=str(self.path))
        turn = client.request('submit', bot='Bob', request_id='file',
                              prompt="shell:printf created > artifact; printf stdout; printf stderr >&2")['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertEqual((self.path / 'artifact').read_text(), 'created')
        events = client.request('events', bot='Bob', after=0, limit=256)['result']['events']
        node = next(e['data']['node'] for e in events if e['event'] == 'tool_completed')
        result = json.loads(client.request('item', bot='Bob', node=node)['result']['output'])
        self.assertEqual((result['stdout'], result['stderr'], result['exit_code']), ('stdout', 'stderr', 0))
        turn = client.request('submit', bot='Bob', request_id='cancel',
                              prompt='shell:sleep 30 & echo $! > child.pid; wait')['result']['turn']
        deadline = time.monotonic() + 3
        while not (self.path / 'child.pid').exists() and time.monotonic() < deadline:
            time.sleep(.01)
        pid = int((self.path / 'child.pid').read_text())
        child = psutil.Process(pid)
        client.request('interrupt', bot='Bob', turn=turn)
        # The shell is killed, but its unrecorded effects remain unknown.
        # The result states that uncertainty and the bot stays usable.
        self.assertEqual(client.finished(turn)['data']['status'], 'interrupted')
        deadline = time.monotonic() + 2
        def alive():
            try:
                return child.is_running() and child.status() != psutil.STATUS_ZOMBIE
            except psutil.NoSuchProcess:
                return False
        while alive() and time.monotonic() < deadline:
            time.sleep(.01)
        self.assertFalse(alive())
        events = client.request('events', bot='Bob', after=0, limit=256)['result']['events']
        killed = [e for e in events if e['event'] == 'tool_completed' and e['turn'] == turn]
        self.assertEqual(len(killed), 1)
        output = json.loads(client.request('item', bot='Bob', node=killed[0]['data']['node'])['result']['output'])
        self.assertEqual(output['error'], 'tool_outcome_unknown')
        self.assertIn('may still be running', output['detail'])
        again = client.request('submit', bot='Bob', request_id='retry', prompt='hi')['result']['turn']
        self.assertEqual(client.finished(again)['data']['status'], 'completed')

    def test_second_owner_cannot_recover_another_process_turn(self):
        client = self.client()
        client.request('create', bot='Bob', workspace=str(self.path))
        turn = client.request('submit', bot='Bob', request_id='running', prompt='wait')['result']['turn']
        self.model.requests.get(timeout=3)
        second = subprocess.run([str(self.binary), *serve_args(self.path / 'state.sqlite', self.url)],
                                input='', capture_output=True, text=True, timeout=5, env=clean_env())
        self.assertEqual(second.returncode, 75)
        self.assertIn('store_already_owned', second.stderr)
        self.assertEqual(client.request('resume', bot='Bob')['result']['status'], 'running')
        client.request('interrupt', bot='Bob', turn=turn)
        client.finished(turn)

    def test_store_aliases_cannot_recover_live_work(self):
        client = self.client()
        client.request('create', bot='Bob', workspace=str(self.path))
        turn = client.request('submit', bot='Bob', request_id='running', prompt='wait')['result']['turn']
        self.model.requests.get(timeout=3)
        for kind in ('symlink', 'hardlink'):
            alias = self.path / (kind + '.sqlite')
            if kind == 'symlink':
                alias.symlink_to(self.path / 'state.sqlite')
            else:
                os.link(self.path / 'state.sqlite', alias)
            try:
                second = subprocess.run([str(self.binary), *serve_args(alias, self.url)], input='',
                    capture_output=True, text=True, timeout=5, env=clean_env())
                self.assertEqual(second.returncode, 75 if kind == 'symlink' else 1, kind)
                self.assertIn('store_already_owned' if kind == 'symlink'
                              else 'store_hard_links_unsupported', second.stderr)
                self.assertEqual(client.request('resume', bot='Bob')['result']['status'], 'running')
            finally:
                alias.unlink()
        client.request('interrupt', bot='Bob', turn=turn)
        client.finished(turn)

    def test_large_event_replay_pages_preserve_all_events_and_service_liveness(self):
        client = self.client()
        client.request('create', bot='Bob', workspace=str(self.path))
        for index in range(3):
            turn = client.request('submit', bot='Bob', request_id=str(index), prompt='large-call-id')['result']['turn']
            self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        cursor, events, nonempty_pages = 0, [], 0
        while True:
            reply = client.request('events', bot='Bob', after=cursor, limit=256)
            self.assertLessEqual(len(json.dumps(reply).encode()), 1024 * 1024)
            page = reply['result']
            if not page['events']:
                break
            self.assertGreater(page['next_cursor'], cursor)
            cursor = page['next_cursor']
            events.extend(page['events'])
            nonempty_pages += 1
        self.assertGreater(nonempty_pages, 1)
        self.assertEqual(len({e['cursor'] for e in events}), len(events))
        self.assertEqual(sum(e['event'] == 'tool_started' for e in events), 3)
        self.assertEqual(sum(e['event'] == 'tool_completed' for e in events), 3)
        self.assertEqual(sum(e['event'] == 'turn_finished' for e in events), 3)
        self.assertEqual(client.request('resume', bot='Bob')['result']['status'], 'completed')

    def test_distinct_store_files_do_not_collide_and_symlink_resume_is_exact(self):
        client = self.client()
        client.request('create', bot='Bob', workspace=str(self.path))
        other = Client(self.binary, self.path / 'state.db', self.url)
        self.addCleanup(other.close)
        self.assertEqual(other.request('resume', bot='Bob')['error'], 'bot_not_found')
        client.close()
        alias = self.path / 'alias.sqlite'
        alias.symlink_to(self.path / 'state.sqlite')
        resumed = Client(self.binary, alias, self.url)
        self.addCleanup(resumed.close)
        self.assertEqual(resumed.request('resume', bot='Bob')['result']['status'], 'idle')
        resumed.close()
        link = self.path / 'hardlink.sqlite'
        os.link(self.path / 'state.sqlite', link)
        rejected = subprocess.run([str(self.binary), *serve_args(link, self.url)], input='',
            capture_output=True, text=True, timeout=5, env=clean_env())
        self.assertEqual(rejected.returncode, 1)
        self.assertIn('store_hard_links_unsupported', rejected.stderr)

    def test_provider_credential_is_excluded_from_shell_environment(self):
        sentinel = 'synthetic-test-value-not-a-credential'
        self.model.expected_authorization = 'Bearer ' + sentinel
        self.model.auth_checks = []
        env = {**clean_env(), 'AGENT_TEST_FAKE_KEY': sentinel, 'AGENT_TEST_ALLOWED': 'preserved'}
        client = Client(self.binary, self.path / 'state.sqlite', self.url, 'echo,shell',
                        key_env='AGENT_TEST_FAKE_KEY', env=env)
        self.addCleanup(client.close)
        client.request('create', bot='Bob', workspace=str(self.path))
        turn = client.request('submit', bot='Bob', request_id='env',
            prompt='shell:printf "%s:%s" "${AGENT_TEST_FAKE_KEY-unset}" "$AGENT_TEST_ALLOWED"')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        events = client.request('events', bot='Bob', after=0, limit=256)['result']['events']
        node = next(e['data']['node'] for e in events if e['event'] == 'tool_completed')
        output = client.request('item', bot='Bob', node=node)['result']['output']
        # Assert booleans so even a failed test never prints the credential value.
        self.assertTrue(json.loads(output)['stdout'] == 'unset:preserved')
        self.assertFalse(sentinel in output)
        # A synthetic fixture also exercises exact-value redaction independently
        # of environment filtering. No actual provider secret is used.
        (self.path / 'synthetic-value.txt').write_text(sentinel)
        turn = client.request('submit', bot='Bob', request_id='redaction',
            prompt='shell:cat synthetic-value.txt')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        events = client.request('events', bot='Bob', after=0, limit=256)['result']['events']
        for event in events:
            if 'node' in event['data']:
                item = client.request('item', bot='Bob', node=event['data']['node'])['result']
                self.assertFalse(sentinel in json.dumps(item))
        self.assertEqual(len(self.model.auth_checks), 4)
        self.assertTrue(all(self.model.auth_checks))

    def test_history_pages_recover_an_omitted_long_message(self):
        client = self.client(tools='echo,history', extra=('--context-items', '6'))
        client.request('create', bot='Bob', workspace=str(self.path))
        prompt = 'é🦀"\\' * 9000 + ' final fact'
        reasoning = {'type': 'reasoning', 'id': 'rs_history',
                     'summary': [{'type': 'summary_text', 'text': 'Résumé 🦀'}],
                     'encrypted_content': 'opaque-state' * 10000}
        for n, text in enumerate((prompt, 'next', 'next again', 'omit the first turn')):
            self.model.history_reasoning = reasoning if n == 0 else None
            self.model.history_multiline = n == 0
            turn = client.request('submit', bot='Bob', request_id=str(n), prompt=text)['result']['turn']
            finished = client.finished(turn)['data']
            self.assertEqual(finished['status'], 'completed', finished)
        seed_requests = [self.model.requests.get(timeout=1) for _ in range(4)]
        self.assertIn(reasoning, seed_requests[1]['input'])
        client.request('shutdown')
        client.close()
        client = self.client(tools='echo,history', extra=('--context-items', '6', '--context-bytes', '65536'))
        offset, pieces, completed_items = 0, [], 0
        while True:
            while not self.model.requests.empty():
                self.model.requests.get()
            # Exercise remaining space after substantial work in this same turn.
            self.model.history_prefill = 56000 if len(pieces) == 1 else 0
            turn = client.request('submit', bot='Bob', request_id=f'page-{offset}',
                                  prompt=f'history:1,{offset}' + (',97' if offset == 0 else ''))['result']['turn']
            finished = client.finished(turn)['data']
            self.assertEqual(finished['status'], 'completed', finished)
            requests = []
            while not self.model.requests.empty():
                requests.append(self.model.requests.get())
            result = next(item for item in reversed(requests[-1]['input'])
                          if item.get('type') == 'function_call_output'
                          and item.get('call_id') == 'history-1')
            page = json.loads(result['output'])
            completed_items += page['items']
            self.assertEqual(page['offset'], offset)
            self.assertLessEqual(len(page['text'].encode()), 97 if offset == 0 else 65536)
            for request in requests:
                self.assertLessEqual(len(json.dumps(request['input'], ensure_ascii=False, separators=(',', ':')).encode()) - 2, 65536)
            pieces.append(page['text'])
            if page['done']:
                break
            self.assertTrue(page['truncated'])
            self.assertGreater(page['next_offset'], offset)
            offset = page['next_offset']
        records = [json.loads(line) for line in ''.join(pieces).splitlines()]
        self.assertEqual(completed_items, len(records))
        self.assertGreater(len(pieces), 1)
        self.assertEqual(records[0]['content'][0]['text'], prompt)
        self.assertEqual(records[1], {k: v for k, v in reasoning.items() if k != 'encrypted_content'})
        self.assertEqual(records[2]['content'][0]['text'], 'reply:' + prompt)

    def test_long_history_is_windowed_at_turn_boundaries_and_readable_by_ordinal(self):
        client = self.client(tools='echo,history', extra=('--context-items', '6'))
        client.request('create', bot='Bob', workspace=str(self.path))
        for n in range(1, 6):
            turn = client.request('submit', bot='Bob', request_id=f'p{n}', prompt=f'p{n}')['result']['turn']
            self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        requests = []
        while not self.model.requests.empty():
            requests.append(self.model.requests.get())
        # Turn 4's request is the first that overflows six items; from then on
        # the model sees an explicit note and the newest whole turns only.
        texts = [[i['content'][0]['text'] for i in r['input'] if i.get('role') == 'user'] for r in requests]
        self.assertEqual(texts[2], ['p1', 'p2', 'p3'])
        self.assertEqual(texts[3][1:], ['p3', 'p4'])
        self.assertTrue(texts[3][0].startswith('[context note] 2 earlier turn(s) with 4 messages'))
        self.assertEqual(texts[4][1:], ['p3', 'p4', 'p5'])  # start held: still fits
        self.assertNotIn('reply:p2', json.dumps(requests[4]['input']))
        self.assertEqual(len(requests[4]['input']), 6)
        # Stored history is complete regardless of the window.
        self.assertEqual(len(client.request('events', bot='Bob', after=0, limit=256)['result']['events']), 5 * 4 + 1)
        turn = client.request('submit', bot='Bob', request_id='h', prompt='history:1')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        final = client.request('result', bot='Bob', turn=turn)['result']['text']
        self.assertTrue(final.startswith('echo:'))
        read = json.loads(final[5:])
        self.assertEqual(read['turn'], 1)
        self.assertIn('p1', read['text'])
        self.assertIn('reply:p1', read['text'])
        self.assertNotIn('p2', read['text'])
        turn = client.request('submit', bot='Bob', request_id='h9', prompt='history:9')['result']['turn']
        client.finished(turn)
        self.assertIn('turn_not_in_history', client.request('result', bot='Bob', turn=turn)['result']['text'])
        # A fork sees the same lineage and computes its own window over it.
        client.request('fork', source='Bob', bot='branch', workspace=str(self.path))
        turn = client.request('submit', bot='branch', request_id='b', prompt='history:2')['result']['turn']
        client.finished(turn)
        self.assertIn('reply:p2', client.request('result', bot='branch', turn=turn)['result']['text'])
        while not self.model.requests.empty():
            requests.append(self.model.requests.get())
        self.assertTrue(requests[-2]['input'][0]['content'][0]['text'].startswith('[context note]'))

    def test_retention_prunes_records_and_deletes_idle_bots(self):
        client = self.client(extra=('--retain-turns', '2'))
        client.request('create', bot='Bob', workspace=str(self.path))
        for n in range(4):
            turn = client.request('submit', bot='Bob', request_id=str(n), prompt=f'p{n}')['result']['turn']
            self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        page = client.request('events', bot='Bob', after=0, limit=256)['result']
        # The policy pruned turns 1 and 2 after turn 4 finished; created plus two turns remain.
        self.assertIn('pruned_before', page)
        self.assertEqual([e['event'] for e in page['events']][:1], ['created'])
        self.assertEqual(len(page['events']), 1 + 2 * 4)
        self.assertNotIn('pruned_before', client.request('events', bot='Bob', after=page['pruned_before'], limit=256)['result'])
        self.assertEqual(len(client.request('turns', bot='Bob')['result']['turns']), 4)
        # Explicit prune goes further; the transcript still answers item reads.
        self.assertEqual(client.request('prune', bot='Bob', keep_turns=1)['result']['events'], 4)
        self.assertEqual(client.request('prune', bot='Bob', keep_turns=0)['error'], 'invalid_retention')
        first = [e for e in page['events'] if e['event'] == 'message'][0]['data']['node']
        self.assertEqual(client.request('item', bot='Bob', node=first)['result']['content'][0]['text'], 'reply:p2')
        # A follower asking for pruned history is told so before the rest.
        client.request('follow', bot='Bob', after=0)
        self.assertEqual(client.receive(lambda m: m.get('event') == 'pruned')['bot'], 'Bob')
        client.receive(lambda m: m.get('event') == 'follow_live')
        # Delete refuses a busy bot, then frees an idle one; followers see it go.
        turn = client.request('submit', bot='Bob', request_id='slow', prompt='wait')['result']['turn']
        self.assertEqual(client.request('delete', bot='Bob')['error'], 'bot_busy')
        client.receive(lambda m: m.get('event') == 'turn_finished' and m.get('turn') == turn, timeout=10)
        freed = client.request('delete', bot='Bob')['result']
        self.assertEqual(freed['turns'], 5)
        self.assertGreater(freed['nodes'], 0)
        self.assertEqual(client.receive(lambda m: m.get('event') == 'deleted')['bot'], 'Bob')
        self.assertEqual(client.request('resume', bot='Bob')['error'], 'bot_not_found')
        self.assertEqual(client.request('delete', bot='Bob')['error'], 'bot_not_found')
        self.assertEqual(client.request('bots')['result']['bots'], [])

    def test_retention_preserves_background_completion_and_stale_turn_identity(self):
        client = self.client(tools='shell,wait', extra=('--retain-turns', '1'))
        client.request('create', bot='Bob', workspace=str(self.path))
        old = client.request('submit', bot='Bob', request_id='bg',
                             prompt='bg:while [ ! -f release ]; do sleep .01; done; printf done')['result']['turn']
        client.finished(old)
        text = client.request('result', bot='Bob', turn=old)['result']['text']
        handle = json.loads(text.removeprefix('echo:'))['handle']
        self.assertEqual(client.request('delete', bot='Bob')['error'], 'bot_busy')
        later = client.request('submit', bot='Bob', request_id='next', prompt='next')['result']['turn']
        client.finished(later)
        self.assertEqual(client.request('result', bot='Bob', turn=old)['error'], 'turn_result_pruned')
        waited = client.request('wait', handles=[f'turn:Bob/{old}'], timeout_ms=100)['result']['results']
        self.assertEqual(waited[f'turn:Bob/{old}']['error'], 'turn_result_pruned')
        (self.path / 'release').touch()
        waited = client.request('wait', handles=[handle], timeout_ms=3000)['result']['results']
        self.assertEqual(waited[handle]['stdout'], 'done')
        self.assertIn('result', client.request('delete', bot='Bob'))
        client.request('shutdown')
        client.close()
        client = self.client(tools='shell,wait', extra=('--retain-turns', '1'))
        for index in range(10):
            client.request('create', bot='Bob', workspace=str(self.path))
            new = client.request('submit', bot='Bob', request_id='r', prompt='replacement')['result']['turn']
            client.finished(new)
            self.assertGreater(new, later)
            result = client.request('wait', handles=[f'turn:Bob/{old}'], timeout_ms=100)['result']['results']
            self.assertEqual(result[f'turn:Bob/{old}']['error'], 'turn_not_found')
            # No retry or sleep after the terminal event should be necessary.
            self.assertIn('result', client.request('delete', bot='Bob'))
            later = new

    def test_large_deletions_run_in_pieces_and_refuse_work_meanwhile(self):
        client = self.client('echo,shell')
        for bot in ('Big', 'Other'):
            client.request('create', bot=bot, workspace=str(self.path))
        for n in range(40):
            turn = client.request('submit', bot='Big', request_id=str(n), prompt='shell:printf big')['result']['turn']
            self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        before = client.request('stats')['result']['store']['operations'].get('delete_bot', {}).get('count', 0)
        freed = client.request('delete', bot='Big')['result']
        self.assertEqual(freed['turns'], 40)
        after = client.request('stats')['result']['store']['operations']['delete_bot']['count']
        # 40 turns of records in pieces of 16, then the turn rows, then nodes, then the bot.
        self.assertGreaterEqual(after - before, 5)
        self.assertEqual(client.request('resume', bot='Big')['error'], 'bot_not_found')
        turn = client.request('submit', bot='Other', request_id='o', prompt='hi')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')

    def test_bot_identities_outlive_names_and_refuse_stale_retries(self):
        client = self.client()
        bob = client.request('create', bot='Bob', workspace=str(self.path))['result']
        first = client.request('submit', bot='Bob', request_id='r7', bot_id=bob['id'], prompt='hello')['result']
        self.assertEqual((first['bot_id'], first['duplicate']), (bob['id'], False))
        checkpoint = client.finished(first['turn'])['data']['checkpoint']
        fork = client.request('fork', source='Bob', checkpoint=checkpoint, bot='Fork', workspace=str(self.path))['result']
        self.assertNotEqual(fork['id'], bob['id'])
        # The request namespace is per identity: the fork never ran r7.
        forked = client.request('submit', bot='Fork', request_id='r7', bot_id=fork['id'], prompt='hello')['result']
        self.assertEqual((forked['duplicate'], forked['bot_id']), (False, fork['id']))
        client.finished(forked['turn'])
        self.assertEqual(client.request('submit', bot='Fork', request_id='r7', bot_id=bob['id'], prompt='hello')['error'],
                         'bot_not_found')
        # Identities survive restart and never move to another name.
        client.close(kill=True)
        client = self.client()
        listed = {b['name']: b['id'] for b in client.request('bots')['result']['bots']}
        self.assertEqual(listed, {'Bob': bob['id'], 'Fork': fork['id']})
        self.assertEqual(client.request('resume', bot='Bob')['result']['id'], bob['id'])
        again = client.request('submit', bot='Bob', request_id='r7', bot_id=bob['id'], prompt='hello')['result']
        self.assertEqual((again['duplicate'], again['turn']), (True, first['turn']))
        # A recycled name is a new identity: a stale retry is refused, a plain one is fresh work.
        client.request('delete', bot='Bob')
        reborn = client.request('create', bot='Bob', workspace=str(self.path))['result']
        self.assertGreater(reborn['id'], fork['id'])
        stale = client.request('submit', bot='Bob', request_id='r7', bot_id=bob['id'], prompt='hello')
        self.assertEqual(stale['error'], 'bot_not_found')
        self.assertIn(str(reborn['id']), stale['detail'])
        fresh = client.request('submit', bot='Bob', request_id='r7', prompt='hello')['result']
        self.assertEqual((fresh['duplicate'], fresh['bot_id']), (False, reborn['id']))
        self.assertNotEqual(fresh['turn'], first['turn'])
        client.finished(fresh['turn'])
        self.assertEqual(client.request('submit', bot='Gone', request_id='r7', bot_id=bob['id'], prompt='hello')['error'],
                         'bot_not_found')

    def test_transient_failures_are_retried_and_rate_limits_pace_the_pool(self):
        client = self.client()
        client.request('create', bot='Bob', workspace=str(self.path))

        def run(prompt, timeout=15):
            response = client.request('submit', bot='Bob', request_id=prompt.replace(':', '-'), prompt=prompt)
            turn = response['result']['turn'] if 'result' in response else self.fail(response)
            finished = client.receive(lambda m: m.get('event') == 'turn_finished' and m.get('turn') == turn, timeout=timeout)
            retries = [m for m in client.saved if m.get('event') == 'retry' and m.get('turn') == turn]
            client.saved.clear()
            return finished['data'], retries

        for prompt, code in (('flaky:1', 'provider_http_503'), ('limited:1', 'provider_http_429'),
                             ('streamlimit:1', 'provider_rate_limited'),
                             ('streamlimit-flat:1', 'provider_rate_limited'),
                             ('origin:1', 'provider_http_520')):
            with self.subTest(prompt=prompt):
                data, retries = run(prompt)
                self.assertEqual(data['status'], 'completed', data)
                self.assertEqual([(r['attempt'], r['error']) for r in retries], [(1, code)])
        turns = client.request('turns', bot='Bob')['result']['turns']
        self.assertEqual([t['retries'] for t in turns], [1, 1, 1, 1, 1])
        self.assertGreaterEqual(turns[1]['paced_ms'], 40)   # the retry waited for the pool's Retry-After
        self.assertGreaterEqual(turns[2]['paced_ms'], 250)  # and for the delay named in the stream
        self.assertGreaterEqual(turns[3]['paced_ms'], 250)  # top-level error uses the same pool delay
        # Headers saying the allowance is spent pace the next call; nothing fails.
        self.assertEqual(run('paced:1')[0]['status'], 'completed')
        data, retries = run('hello')
        self.assertEqual((data['status'], retries), ('completed', []))
        held = client.request('turns', bot='Bob')['result']['turns'][-1]
        self.assertEqual(held['retries'], 0)
        self.assertGreaterEqual(held['paced_ms'], 300)
        # A response the model could not finish is final, not retried.
        data, retries = run('incomplete')
        self.assertEqual((data['status'], data['error'], retries), ('failed', 'provider_incomplete', []))
        # A refusal that never lifts is bounded: 64 paced attempts, each held
        # for the provider's Retry-After, then the turn fails with the refusal.
        data, retries = run('limited-forever:1', timeout=60)
        self.assertEqual((data['status'], data['error']), ('failed', 'provider_http_429'))
        self.assertEqual(len(retries), 63)
        self.assertEqual(client.request('turns', bot='Bob')['result']['turns'][-1]['retries'], 63)

    def test_premature_provider_eof_cannot_be_a_successful_checkpoint(self):
        client = self.client()
        client.request('create', bot='Bob', workspace=str(self.path))
        turn = client.request('submit', bot='Bob', request_id='truncated', prompt='truncate')['result']['turn']
        event = client.finished(turn)
        self.assertEqual(event['data']['status'], 'failed')
        self.assertIsNone(event['data']['checkpoint'])
        self.assertEqual(event['data']['error'], 'missing_completion')
