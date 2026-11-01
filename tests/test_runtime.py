"""Actual Rust process, disk recovery, provider transport, and tool loop."""
from contextlib import closing
import hashlib
import http.server
import json
import os
from pathlib import Path
import queue
import re
import signal
import sqlite3
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import unittest

from bench.targets import clean_env
from bench.runtime_client import PROTECTING, Client, node_item, poll, serve_args

# Longer than 256 bytes, with text a shell would run if pasted unquoted.
ODD_CALL_ID = "odd $(touch pwned) 'x'\n" + 'L' * 300
# The line Sonny ends a message to a bot with: when it was sent.
SENT = re.compile(r'\n\n\[Sent [A-Z][a-z]{2} \d{1,2} [A-Z][a-z]{2} \d{4} \d{2}:\d{2}, UTC[+-]\d{2}:\d{2}\.\]$')



def input_tokens(server, request):
    """What the provider counts for a request: 100, or with
    `bytes_per_token` set, its encoded size in those."""
    per = getattr(server, 'bytes_per_token', None)
    return len(json.dumps(request)) // per if per else 100


def is_summary(request):
    """A summary request in either form: a copy of the bot's call, or a
    request of its own over a span. Both end in the compaction request."""
    items = request.get('input') or request.get('messages') or []
    content = items[-1].get('content') if items else None
    return (isinstance(content, list) and bool(content) and items[-1].get('role') == 'user'
            and content[-1].get('text', '').startswith('[compaction request]'))

class Model(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def log_message(self, *_):
        pass

    def do_GET(self):
        # A listing only when a test gives the model a context window.
        window = getattr(self.server, 'context_window', None)
        if not self.path.endswith('/models') or window is None:
            self.send_response(404)
            self.send_header('Content-Length', '0')
            self.end_headers()
            return
        models = getattr(self.server, 'models', None) or ('synthetic-model',)
        names = getattr(self.server, 'model_names', {})
        listed = [{'id': m, 'context_window': window} | ({'display_name': names[m]} if m in names else {})
                  for m in models]
        body = json.dumps({'data': listed}).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        try:
            body = self.rfile.read(int(self.headers['Content-Length']))
            request = json.loads(body)
            self.server.requests.put(request)
            if hasattr(self.server, 'bodies'):
                self.server.bodies.append(body)
            if hasattr(self.server, 'hold_after_summary'):
                # Work calls sent after the first summary, or just before
                # it, beside it, wait for the test.
                if is_summary(request):
                    self.server.summarized = True
                elif time.sleep(.1) or getattr(self.server, 'summarized', False):
                    self.server.hold_after_summary.wait(timeout=10)
            if hasattr(self.server, 'timeline'):
                # When each request arrived, and for a summary, when it ended
                # after `summary_delay`.
                started = time.monotonic()
                if is_summary(request):
                    self.server.summarizing = True
                    time.sleep(getattr(self.server, 'summary_delay', 0))
                    self.server.summarizing = False
                self.server.timeline.append((is_summary(request), started, time.monotonic()))
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
            if getattr(self.server, 'reject_compaction', False) and is_summary(request):
                body = b'{"error":{"message":"synthetic compaction refusal"}}'
                self.send_response(400)
                self.send_header('Content-Length', str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                self.wfile.flush()
                return
            if is_summary(request) and getattr(self.server, 'compaction_refusals', 0):
                self.server.compaction_refusals -= 1
                body = b'{"error":{"message":"synthetic compaction rate limit"}}'
                self.send_response(429)
                self.send_header('Content-Length', str(len(body)))
                # Exhaust retries quickly, then force the ordinary call to park.
                last = getattr(self.server, 'compaction_retry_after', '0.4')
                self.send_header('Retry-After', last if self.server.compaction_refusals == 0 else '0.001')
                self.end_headers()
                self.wfile.write(body)
                self.wfile.flush()
                return
            assert self.path == '/v1/responses'
            assert request['model'] in getattr(self.server, 'models', ('synthetic-model',))
            texts = [i['content'][0]['text'] for i in request['input'] if i.get('role') == 'user']
            # A `steer:` message joins the running task, which its prompt drives.
            user = next((t for t in reversed(texts) if not t.startswith('steer:')), texts[-1])
            # A trigger's message comes after its one-line header.
            if user.startswith('[trigger ') and '\n' in user:
                user = user.split('\n', 1)[1]
            # Sonny's message ends with a line naming when it was sent.
            user = SENT.sub('', user)
            if user in getattr(self.server, 'refused_prompts', ()):
                # A refusal with no usage: the turn fails without a model round.
                body = b'{"error":{"message":"synthetic refusal"}}'
                self.send_response(400)
                self.send_header('Content-Length', str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                self.wfile.flush()
                return
            attempts = getattr(self.server, 'attempts', {})
            attempt = attempts[user] = attempts.get(user, 0) + 1
            self.server.attempts = attempts
            # When each attempt reached the model, by this process's clock.
            arrived = getattr(self.server, 'arrived', {})
            arrived[user, attempt] = time.monotonic()
            self.server.arrived = arrived
            pace_at = getattr(self.server, 'pace_at', None)
            if (pace_at is not None and not is_summary(request)
                    and sum(i.get('type') == 'function_call_output' for i in request['input']) >= pace_at):
                # Once, after that many results: a 429 that parks the turn.
                self.server.pace_at = None
                body = b'{"error":{"message":"try later"}}'
                self.send_response(429)
                self.send_header('Content-Type', 'application/json')
                self.send_header('Content-Length', str(len(body)))
                self.send_header('Retry-After', '0.3')
                self.end_headers()
                self.wfile.write(body)
                return
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
            if ((user.startswith(('flaky:', 'origin:', 'limited:', 'waitretry:', 'busy:')) and attempt == 1)
                    or (user.startswith('outage:') and attempt <= int(user.split(':')[1]))
                    or user.startswith('limited-forever:') or (user == 'tool:limited' and attempt == 2)):
                # Transport-level refusals: a 503 or a CDN's 520 the next
                # attempt clears, a 429 with Retry-After (for tool:limited, on
                # the call after the tool), or a 429 that never lifts.
                body = json.dumps({'error': {'message': 'try later'}}).encode()
                self.send_response(520 if user.startswith('origin:') else
                                   503 if user.startswith(('flaky:', 'waitretry:', 'outage:', 'busy:')) else 429)
                self.send_header('Content-Type', 'application/json')
                self.send_header('Content-Length', str(len(body)))
                if user.startswith('busy:'):
                    # A 503 that says when to come back.
                    self.send_header('Retry-After', '0.7')
                elif not user.startswith(('flaky:', 'origin:', 'waitretry:', 'outage:')):
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
            elif (respond := getattr(self.server, 'respond', None)) and (said := respond(request, user)) is not None:
                # A test's own model: calls as [[name, arguments], ...], or
                # an answer in words.
                text = '' if isinstance(said, list) else said
                output = ([{'type': 'function_call', 'name': name, 'call_id': f'{name}-{n + 1}',
                            'arguments': json.dumps(arguments)} for n, (name, arguments) in enumerate(said)]
                          if isinstance(said, list) else
                          [{'type': 'message', 'role': 'assistant', 'content': [{'type': 'output_text', 'text': said}]}])
            elif getattr(self.server, 'task_script', None) is not None:
                # A scripted agent: one shell command a model call, then an
                # answer quoting every throughput the last result reported,
                # each with its close.
                step = self.server.task_step
                if not is_summary(request):
                    self.server.task_step += 1
                text = ''
                if step < len(self.server.task_script):
                    output = [{'type': 'function_call', 'name': 'shell', 'call_id': f'task-{step}',
                               'arguments': json.dumps({'command': self.server.task_script[step]})}]
                else:
                    found = re.findall(r'(?:close (\S+) )?throughput: ([0-9]+)', last.get('output', ''))
                    numbers = ', '.join(f'{close} {number}'.strip() for close, number in found)
                    text = f"Done. make bench reports throughput: {numbers or '?'} rows/s."
                    output = [{'type': 'message', 'role': 'assistant',
                               'content': [{'type': 'output_text', 'text': text}]}]
            elif last.get('type') == 'function_call_output' and user.startswith('bgwait:') and '"handle"' in last['output']:
                # Second step of a start-then-wait turn: park on the process handle.
                text = ''
                output = [{'type': 'function_call', 'name': 'wait', 'call_id': 'wait-1',
                           'arguments': json.dumps({'handles': [json.loads(last['output'])['handle']]})}]
            elif user.startswith('long:'):
                # One long task: a shell call a round, each result about
                # 11 KiB and each appending its round to rounds.log, then a
                # read of the first elided result, then done.
                # `long:COUNTxLINES,...` sets each round's lines instead.
                # Progress is the newest call since the prompt: a cut inside
                # the turn summarizes older rounds but keeps the prompt.
                start = max(n for n, i in enumerate(request['input']) if i.get('role') == 'user'
                            and not i['content'][0]['text'].startswith('steer:'))
                calls = [i for i in request['input'][start:] if i.get('type') == 'function_call']
                done = max((int(c['call_id'][5:]) + 1 for c in calls if c['call_id'][5:].isdigit()), default=0)
                stubs = [i for i in request['input'] if i.get('type') == 'function_call_output'
                         and i['output'].startswith('[tool result elided')]
                spec = user[5:]
                sizes = ([600] * int(spec) if 'x' not in spec else
                         [int(lines) for part in spec.split(',')
                          for count, lines in [part.split('x')] for _ in range(int(count))])
                text = ''
                # With `long_wait`, the rounds end by starting that command
                # in the background and waiting on it.
                waited = getattr(self.server, 'long_wait', None)
                named = {c['call_id'] for c in calls}
                if done >= len(sizes) and waited and 'long-bg' not in named:
                    output = [{'type': 'function_call', 'name': 'shell', 'call_id': 'long-bg',
                               'arguments': json.dumps({'command': waited, 'timeout_ms': 5000, 'background': True})}]
                elif done >= len(sizes) and waited and 'long-wait' not in named:
                    output = [{'type': 'function_call', 'name': 'wait', 'call_id': 'long-wait',
                               'arguments': json.dumps({'handles': [json.loads(last['output'])['handle']]})}]
                elif done < len(sizes):
                    output = [{'type': 'function_call', 'name': 'shell', 'call_id': f'long-{done}',
                               'arguments': json.dumps({'command': f"seq -f 'round {done} line %g' 1 {sizes[done]}; echo {done} >> rounds.log",
                                                        'timeout_ms': 5000})}]
                elif stubs and not any(c['name'] == 'read' for c in calls):
                    reference = re.search(r'artifact "(result/[0-9]+)"', stubs[0]['output']).group(1)
                    output = [{'type': 'function_call', 'name': 'read', 'call_id': 'long-read',
                               'arguments': json.dumps({'artifact': reference})}]
                else:
                    text = f'done after {done} rounds'
                    output = [{'type': 'message', 'role': 'assistant',
                               'content': [{'type': 'output_text', 'text': text}]}]
            elif user == 'script' and getattr(self.server, 'call_script', None):
                # One round a step of `call_script`, then an answer; a cut
                # inside the turn keeps at least its newest call in view. A
                # step is a call, or a list of calls made in one round.
                done = max((int(i['call_id'][7:].split('.')[0]) + 1 for i in request['input']
                            if i.get('type') in ('function_call', 'custom_tool_call')
                            and i['call_id'].startswith('script-')),
                           default=0)
                text = ''
                if done < len(self.server.call_script):
                    step = self.server.call_script[done]
                    calls = step if isinstance(step, list) else [step]
                    # Text arguments make a custom tool's call.
                    output = [{'type': 'custom_tool_call', 'name': name, 'input': arguments}
                              if isinstance(arguments, str) else
                              {'type': 'function_call', 'name': name, 'arguments': json.dumps(arguments)}
                              for name, arguments in calls]
                    for n, item in enumerate(output):
                        item['call_id'] = f'script-{done}' + (f'.{n}' if n else '')
                else:
                    text = 'done'
                    output = [{'type': 'message', 'role': 'assistant',
                               'content': [{'type': 'output_text', 'text': text}]}]
            elif user.startswith(('rewait:', 'rewait2:', 'liftwait:')):
                # Calls this turn: a `liftwait:` command in the foreground,
                # then a wait on any handle a result names, again while one
                # is pending, then the last result echoed.
                start = max(n for n, i in enumerate(request['input']) if i.get('role') == 'user'
                            and not i['content'][0]['text'].startswith('steer:'))
                outputs = [json.loads(i['output']) for i in request['input'][start:]
                           if i.get('type') == 'function_call_output']
                calls = sum(i.get('type') == 'function_call' for i in request['input'][start:])
                handle = next((o['handle'] for o in outputs if 'handle' in o), user.split(':', 1)[1])
                text = ''
                if user.startswith('liftwait:') and not outputs:
                    output = [{'type': 'function_call', 'name': 'shell', 'call_id': 'lift-1',
                               'arguments': json.dumps({'command': user[9:], 'timeout_ms': 5000})}]
                elif user.startswith('rewait2:') and not outputs:
                    # Two waits in one response.
                    output = [{'type': 'function_call', 'name': 'wait', 'call_id': f'wait-{n}',
                               'arguments': json.dumps({'handles': [handle]})} for n in (1, 2)]
                elif not outputs or outputs[-1].get('pending') or 'handle' in outputs[-1]:
                    output = [{'type': 'function_call', 'name': 'wait', 'call_id': f'wait-{calls + 1}',
                               'arguments': json.dumps({'handles': [handle]})}]
                else:
                    text = 'echo:' + json.dumps(outputs[-1])
                    output = [{'type': 'message', 'role': 'assistant',
                               'content': [{'type': 'output_text', 'text': text}]}]
            elif user.startswith('para:'):
                # `para:CMD|CMD|...`: every command in one response, then
                # the results in the order they were recorded.
                start = max(n for n, i in enumerate(request['input']) if i.get('role') == 'user'
                            and not i['content'][0]['text'].startswith('steer:'))
                outputs = [i for i in request['input'][start:] if i.get('type') == 'function_call_output']
                text = ''
                if not outputs:
                    output = [{'type': 'function_call', 'name': 'shell', 'call_id': f'para-{n}',
                               'arguments': json.dumps({'command': command, 'timeout_ms': 5000})}
                              for n, command in enumerate(user[5:].split('|'), 1)]
                else:
                    text = 'echo:' + json.dumps([i['call_id'] for i in outputs])
                    output = [{'type': 'message', 'role': 'assistant',
                               'content': [{'type': 'output_text', 'text': text}]}]
            elif user.startswith('sayslow:'):
                # An answer and a slow command in one response, then a word
                # for whatever steered it.
                start = max(n for n, i in enumerate(request['input']) if i.get('role') == 'user'
                            and not i['content'][0]['text'].startswith('steer:'))
                if not any(i.get('type') == 'function_call_output' for i in request['input'][start:]):
                    text = user[8:]
                    output = [{'type': 'message', 'role': 'assistant',
                               'content': [{'type': 'output_text', 'text': text}]},
                              {'type': 'function_call', 'name': 'shell', 'call_id': 'slow-1',
                               'arguments': json.dumps({'command': getattr(self.server, 'slow_command', 'sleep 2')})}]
                else:
                    text = 'heard'
                    output = [{'type': 'message', 'role': 'assistant',
                               'content': [{'type': 'output_text', 'text': text}]}]
            elif user == 'cached:reused-call':
                text = ''
                output = [{'type': 'function_call', 'name': 'echo', 'call_id': 'same-id',
                           'arguments': '{"text":"ok"}'}]
            elif last.get('type') == 'function_call_output':
                # A result with pictures is a list of parts: its text echoes.
                result = last['output']
                text = 'echo:' + (result if isinstance(result, str) else
                                  ''.join(part.get('text', '') for part in result))
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
            elif user.startswith('readpic:') and last.get('type') != 'function_call_output':
                text = ''
                output = [{'type': 'function_call', 'name': 'read', 'call_id': 'readpic-1',
                           'arguments': json.dumps({'path': user[8:]})}]
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
            elif user.startswith('multi:'):
                # One response planning several commands, in order.
                text = ''
                output = [{'type': 'function_call', 'name': 'shell', 'call_id': f'shell-{i + 1}',
                           'arguments': json.dumps({'command': command, 'timeout_ms': 2000})}
                          for i, command in enumerate(user[6:].split('|'))]
            elif user.startswith('waitshell:'):
                # A wait, then a command, in one response.
                text = ''
                handle, _, command = user[10:].partition('|')
                output = [{'type': 'function_call', 'name': 'wait', 'call_id': 'wait-1',
                           'arguments': json.dumps({'handles': [handle]})},
                          {'type': 'function_call', 'name': 'shell', 'call_id': 'shell-1',
                           'arguments': json.dumps({'command': command, 'timeout_ms': 2000})}]
            elif user.startswith('shell:'):
                text = ''
                output = [{'type': 'function_call', 'name': 'shell', 'call_id': 'shell-1',
                           'arguments': json.dumps({'command': user[6:], 'timeout_ms': 2000})}]
            elif user.startswith('calls:'):
                # Calls given whole, as [[name, arguments], ...], in one response.
                text = ''
                output = [{'type': 'function_call', 'name': name, 'call_id': f'{name}-{i + 1}',
                           'arguments': json.dumps(arguments)}
                          for i, (name, arguments) in enumerate(json.loads(user[6:]))]
            elif user.startswith('shellecho:'):
                # A command, then an echo, in one response.
                text = ''
                command, _, echoed = user[10:].partition('|')
                output = [{'type': 'function_call', 'name': 'shell', 'call_id': 'shell-1',
                           'arguments': json.dumps({'command': command, 'timeout_ms': 2000})},
                          {'type': 'function_call', 'name': 'echo', 'call_id': 'echo-1',
                           'arguments': json.dumps({'text': echoed})}]
            elif user.startswith('echoshell:'):
                # An echo, then a command, in one response.
                text = ''
                echoed, _, command = user[10:].partition('|')
                output = [{'type': 'function_call', 'name': 'echo', 'call_id': 'echo-1',
                           'arguments': json.dumps({'text': echoed})},
                          {'type': 'function_call', 'name': 'shell', 'call_id': 'shell-1',
                           'arguments': json.dumps({'command': command, 'timeout_ms': 2000})}]
            elif user.startswith('oddshell:'):
                # A long call id a shell would read as syntax.
                text = ''
                output = [{'type': 'function_call', 'name': 'shell', 'call_id': ODD_CALL_ID,
                           'arguments': json.dumps({'command': user[9:], 'timeout_ms': 2000})}]
            elif user.startswith('tool:'):
                text = ''
                output = [{'type': 'function_call', 'name': 'echo', 'call_id': 'echo-1',
                           'arguments': json.dumps({'text': getattr(self.server, 'note_text', user[5:])})}]
            elif user == 'large-call-id':
                text = ''
                output = [{'type': 'function_call', 'name': 'echo', 'call_id': 'c' * 65536,
                           'arguments': json.dumps({'text': 'ok'})}]
            else:
                reply_for = getattr(self.server, 'reply_for', None)
                text = (reply_for and reply_for(request, user)) or getattr(self.server, 'reply_text', 'reply:' + user)
                output = [{'id': 'msg_text', 'type': 'message', 'role': 'assistant',
                           'content': [{'type': 'output_text', 'text': text}]}]
            if is_summary(request):
                text = getattr(self.server, 'compaction_text', 'A short synthetic summary.')
                output = [{'type': 'message', 'role': 'assistant',
                           'content': [{'type': 'output_text', 'text': text}]}]
                if getattr(self.server, 'compaction_call', False) and request.get('tools'):
                    # A copy offers the bot's tools, and a model may call one.
                    output.append({'type': 'function_call', 'name': 'echo', 'call_id': 'summary-call',
                                   'arguments': json.dumps({'text': 'instead'})})
            if request.get('tool_choice') == 'none' and any(o.get('type') == 'function_call' for o in output):
                # A model shown tools it may not call answers in words.
                text = 'final reply'
                output = [{'type': 'message', 'role': 'assistant',
                           'content': [{'type': 'output_text', 'text': text}]}]
            if getattr(self.server, 'history_reasoning', None):
                output.insert(0, self.server.history_reasoning)
            if getattr(self.server, 'empty_compaction', False) and is_summary(request):
                text, output = '', []
            events = [{'type': 'response.created', 'response': {'id': 'response_test'}}]
            if text:
                events.append({'type': 'response.output_text.delta', 'delta': text})
            if user != 'truncate':
                # A dated snapshot answers, as providers name it.
                events.append({'type': 'response.completed', 'response': {
                    'model': request['model'] + '-2026-09-26', 'status': 'completed', 'output': output,
                    'usage': {'input_tokens': input_tokens(self.server, request), 'output_tokens': 10,
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
            if getattr(self.server, 'fail_billed_at', None) == attempt:
                # A billed attempt the provider then fails, to be retried.
                events = [{'type': 'response.failed', 'response': {'status': 'failed',
                    'usage': {'input_tokens': 100, 'output_tokens': 10}, 'error': {
                    'code': 'rate_limit_exceeded', 'message': 'Please try again in 1ms.'}}}]
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
                deltas = [{'type': 'response.output_text.delta', 'delta': ''}] * 20000
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


def thinking_binding(request, before, host):
    """What a synthetic signature binds to, as newer Claude models do: the
    model, the endpoint that holds the key (here the Host header), the system
    prompt, the tool set, and every message before the block, minus earlier
    thinking blocks and cache markers."""
    def plain(value):
        if isinstance(value, dict):
            return {k: plain(v) for k, v in value.items() if k != 'cache_control'}
        if isinstance(value, list):
            return [plain(v) for v in value]
        return value
    messages = [{**m, 'content': [b for b in m['content'] if b['type'] not in ('thinking', 'redacted_thinking')]}
                for m in before]
    bound = [request['model'], host, plain(request.get('system')),
             sorted(t['name'] for t in request.get('tools', [])), plain(messages)]
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
            # No request asks for a server-side fallback, which would stick;
            # a refused request's own retry redeems the refusal's credit.
            assert 'fallbacks' not in request
            assert self.headers.get('anthropic-beta') == (
                'thinking-binding-controls-2026-08-01,fallback-credit-2026-07-01,'
                'server-side-fallback-2026-07-01,fast-mode-2026-02-01')
            if 'fallback_credit_token' in request:
                assert request['fallback_credit_token'] == 'credit-1' and request['model'] == 'synthetic-claude-2'
                self.server.credits = getattr(self.server, 'credits', 0) + 1
            if 'thinking' in request:
                assert request['thinking']['block_binding'] == {'prefix_mismatch_behavior': 'drop_block'}
            # A cache refresh is the same request with no output and no stream.
            warm = request['max_tokens'] == 0
            assert request['model'] in ('synthetic-claude', 'synthetic-claude-2') and request['stream'] != warm
            cache = getattr(self.server, 'cache_control', {'type': 'ephemeral'})
            for block in request.get('system', []):
                assert block['text'] and block['cache_control'] == cache
            assert request['cache_control'] == cache
            # The API reads breakpoints in order (tools, system, messages,
            # then the request's own last one) and refuses a longer lifetime
            # after a shorter one.
            marks = [b['cache_control'] for b in request.get('tools', []) + request.get('system', [])
                     + [b for m in request['messages'] for b in m['content']] if 'cache_control' in b]
            ttls = [{'5m': 300, '1h': 3600}[m.get('ttl', '5m')] for m in marks + [request['cache_control']]]
            if any(later > earlier for earlier, later in zip(ttls, ttls[1:])):
                body = (b'{"type":"error","error":{"type":"invalid_request_error","message":'
                        b'"a ttl=1h cache_control block must not come after a ttl=5m cache_control block"}}')
                self.send_response(400)
                self.send_header('Content-Length', str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                self.wfile.flush()
                return
            summary = is_summary(request)
            # A summary of its own disables tool calls; a copy of the bot's
            # call keeps its tool choice, which the message cache covers.
            own = request.get('system', [{}])[0].get('text') == 'Summarize.'
            history_uses_tools = any(b['type'] in ('tool_use', 'tool_result')
                                     for m in request['messages'] for b in m['content'])
            if (history_uses_tools and not request.get('tools')) or (
                    own and request.get('tools') and request.get('tool_choice') != {'type': 'none'}):
                body = b'{"error":{"message":"tool history needs definitions; summarization must disable tool calls"}}'
                self.send_response(400)
                self.send_header('Content-Length', str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                self.wfile.flush()
                return
            if not own:
                assert 'tool_choice' not in request
            assert [t['name'] for t in request['tools']][:2] == ['echo', 'shell']
            assert 'input_schema' in request['tools'][0]
            last = request['messages'][-1]
            # Only a turn paused after a hosted search ends with the answer.
            assert last['role'] == 'user' or last['content'][1]['type'] == 'server_tool_use'
            if summary and getattr(self.server, 'compaction_refusals', 0):
                # As on Responses: a rate limit whose last wait parks the turn.
                self.server.compaction_refusals -= 1
                body = b'{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}'
                self.send_response(429)
                self.send_header('Content-Length', str(len(body)))
                last = getattr(self.server, 'compaction_retry_after', '0.4')
                self.send_header('Retry-After', last if self.server.compaction_refusals == 0 else '0.001')
                self.end_headers()
                self.wfile.write(body)
                self.wfile.flush()
                return
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
            signature = getattr(self.server, 'signature', 'sig-1')
            # Simulate replay failures: `unreadable` names the signatures,
            # `unreadable_error` the HTTP status and message, or 'stream'
            # for an api_error after the stream began. An optional failure
            # count also models a transient outage with the same request,
            # and `unreadable_any` a failure whatever the request replays.
            # The fallback model's pool closes long enough to park the turn.
            if request['model'] == 'synthetic-claude-2' and getattr(self.server, 'limit_fallback', 0):
                self.server.limit_fallback -= 1
                body = b'{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}'
                self.send_response(429)
                self.send_header('Retry-After', '1.2')
                self.send_header('Content-Length', str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                self.wfile.flush()
                return
            if getattr(self.server, 'rate_limits', 0):
                self.server.rate_limits -= 1
                body = b'{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}'
                self.send_response(429)
                self.send_header('Retry-After', '0.001')
                self.send_header('Content-Length', str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                self.wfile.flush()
                return
            failure = getattr(self.server, 'unreadable_error', None)
            remaining = getattr(self.server, 'unreadable_failures', None)
            if failure and remaining != 0 and (getattr(self.server, 'unreadable_any', False) or any(
                    b['type'] == 'thinking' and b['signature'] in self.server.unreadable
                    for m in request['messages'] for b in m['content'])):
                if remaining is not None:
                    self.server.unreadable_failures -= 1
                status, message = failure
                kind = {400: 'invalid_request_error', 403: 'permission_error'}.get(status, 'api_error')
                body = json.dumps({'type': 'error', 'error': {'type': kind, 'message': message}})
                if status == 'stream':
                    frames = [('message_start', {'message': {'model': request['model'], 'usage': {'input_tokens': 5}}}),
                              ('error', json.loads(body))]
                    self.send_response(200)
                    self.send_header('Content-Type', 'text/event-stream')
                    self.send_header('Transfer-Encoding', 'chunked')
                    self.end_headers()
                    for kind, data in frames:
                        frame = f'event: {kind}\ndata: {json.dumps({"type": kind, **data})}\n\n'.encode()
                        self.wfile.write(f'{len(frame):x}\r\n'.encode() + frame + b'\r\n')
                    self.wfile.write(b'0\r\n\r\n')
                else:
                    self.send_response(status)
                    self.send_header('Content-Type', 'application/json')
                    self.send_header('Content-Length', str(len(body)))
                    self.end_headers()
                    self.wfile.write(body.encode())
                self.wfile.flush()
                return
            if getattr(self.server, 'bind_thinking', False):
                for index, message in enumerate(request['messages']):
                    for block in message['content']:
                        if block['type'] == 'thinking' and block['signature'] != thinking_binding(
                                request, request['messages'][:index], self.headers.get('Host')):
                            self.server.binding_errors.append(index)
                            body = json.dumps({'type': 'error', 'error': {'type': 'invalid_request_error',
                                'message': f'messages.{index}: The block is bound to a different conversation.'}}).encode()
                            self.send_response(400)
                            self.send_header('Content-Length', str(len(body)))
                            self.end_headers()
                            self.wfile.write(body)
                            self.wfile.flush()
                            return
                signature = thinking_binding(request, request['messages'], self.headers.get('Host'))
            blocks = [{'type': 'thinking', 'thinking': 'plan', 'signature': signature}]
            details = None
            prompt = next((b['text'] for m in request['messages'] if m['role'] == 'user'
                           for b in m['content'] if b['type'] == 'text' and b['text'].startswith('long:')), '')
            if prompt and not summary:
                # The Messages form of the long task: shell rounds, then done.
                start = max(n for n, m in enumerate(request['messages'])
                            if any(b['type'] == 'text' and b['text'] == prompt for b in m['content']))
                calls = max((int(b['id'][11:]) + 1 for m in request['messages'][start:]
                             for b in m['content'] if b['type'] == 'tool_use'
                             and b['id'].startswith('toolu_long_')), default=0)
                # `long:COUNTxLINES,...` sets each round's lines, as on Responses.
                spec = prompt[5:]
                sizes = ([600] * int(spec) if 'x' not in spec else
                         [int(lines) for part in spec.split(',')
                          for count, lines in [part.split('x')] for _ in range(int(count))])
                if calls < len(sizes):
                    blocks.append({'type': 'tool_use', 'id': f'toolu_long_{calls}', 'name': 'shell',
                                   'input': {'command': f"seq -f 'round {calls} line %g' 1 {sizes[calls]}"}})
                    stop = 'tool_use'
                else:
                    blocks.append({'type': 'text', 'text': f'done after {calls} rounds'})
                    stop = 'end_turn'
            elif last['role'] == 'assistant':
                # A paused turn, resent as it stood: finish the answer.
                blocks.append({'type': 'text', 'text': 'resumed'})
                stop = 'end_turn'
            elif last['content'][0]['type'] == 'tool_result':
                # A result that says 'silent' is answered with nothing at all,
                # as a reply after a reaction with nothing to add.
                if last['content'][0]['content'].startswith('silent'):
                    blocks = []
                elif last['content'][0]['content'].startswith('refuse:all'):
                    # What a tool read sets the classifier off.
                    stop = 'refusal'
                    details = {'type': 'refusal', 'category': 'cyber', 'explanation': None}
                else:
                    blocks.append({'type': 'text', 'text': 'echo:' + last['content'][0]['content']})
                stop = 'refusal' if details else 'end_turn'
            else:
                user = SENT.sub('', last['content'][0]['text'])
                if user == 'think-only':
                    stop = 'end_turn'
                elif user == 'silent':
                    blocks = []
                    stop = 'end_turn'
                elif user.startswith('tool:'):
                    blocks.append({'type': 'tool_use', 'id': 'toolu_1', 'name': 'echo', 'input': {'text': user[5:]}})
                    stop = 'tool_use'
                elif user.startswith('shell:'):
                    blocks.append({'type': 'tool_use', 'id': 'toolu_1', 'name': 'shell',
                                   'input': {'command': user[6:]}})
                    stop = 'tool_use'
                elif user.startswith('search:'):
                    # The hosted search runs here, and the server pauses the
                    # turn after it, as a long search does.
                    blocks += [{'type': 'server_tool_use', 'id': 'srvtoolu_1', 'name': 'web_search',
                                'input': {'query': user[7:]}},
                               {'type': 'web_search_tool_result', 'tool_use_id': 'srvtoolu_1',
                                'content': [{'type': 'web_search_result', 'url': 'https://example.test',
                                             'title': 'Example', 'encrypted_content': 'opaque'}]},
                               {'type': 'text', 'text': 'found'}]
                    stop = 'pause_turn'
                elif user.startswith('refuse:') and (request['model'] == 'synthetic-claude'
                                                     or user.startswith('refuse:all')):
                    # A classifier declines mid-output: the partial is
                    # discarded, and the refusal names its category and,
                    # on the first model, a credit for the retry.
                    blocks.append({'type': 'text', 'text': 'Partial '})
                    stop = 'refusal'
                    details = {'type': 'refusal', 'category': 'cyber', 'explanation': None}
                    if request['model'] == 'synthetic-claude':
                        details['fallback_credit_token'] = 'credit-1'
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
                start['input_transformations'] = [{'type': 'thinking_dropped', 'message_index': 1, 'block_index': 0,
                                                   'reason': 'prefix_binding_mismatch'}
                                                  for _ in range(self.server.report_drops)]
            events = [('message_start', {'message': start})]
            for index, block in enumerate(blocks):
                if block['type'] == 'fallback' or block['type'].endswith('_tool_result'):
                    events.append(('content_block_start', {'index': index, 'content_block': block}))
                    events.append(('content_block_stop', {'index': index}))
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
            delta = {'stop_reason': stop, **({'stop_details': details} if details else {})}
            events.append(('message_delta', {'delta': delta, 'usage': usage}))
            events.append(('message_stop', {}))
            # An overload after the stream began, as Anthropic sends one.
            if getattr(self.server, 'overloads', 0):
                self.server.overloads -= 1
                events = [events[0], ('error', {'error': {'type': 'overloaded_error', 'message': 'Overloaded'}})]
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
    def start(self, extra=(), settings=None):
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
                        extra=extra, settings=settings)
        self.addCleanup(client.close)
        return client, model, path

    def test_a_long_tool_call_keeps_the_prompt_cache_warm(self):
        client, model, path = self.start(settings={'keep_warm': 1})
        client.request('create', bot='Bob', workspace=str(path), effort='low')
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

    def test_a_drain_sends_no_refresh_while_a_command_finishes(self):
        client, model, path = self.start(settings={'keep_warm': 1})
        client.request('create', bot='Bob', workspace=str(path), effort='low')
        turn = client.request('submit', bot='Bob', request_id='w1', prompt='shell:sleep 2.5')['result']['turn']
        client.receive(lambda m: m.get('event') == 'tool_started' and m.get('turn') == turn)
        model.requests.get(timeout=3)
        # The command runs on through the drain, past when its cache would be
        # refreshed twice; no refresh goes out, and the turn parks after it.
        self.assertTrue(client.request('shutdown', grace_ms=60_000)['result']['shutting_down'])
        parked = client.receive(lambda e: e.get('event') == 'turn_paced' and e.get('turn') == turn)
        self.assertEqual(parked['data'], {'resume_at_ms': None, 'stopped': True})
        self.assertEqual(client.process.wait(timeout=10), 0)
        self.assertTrue(model.requests.empty())
        self.assertFalse([m for m in client.saved if m.get('event') == 'usage'
                          and m['data'].get('purpose') == 'keep_warm'])

    def test_refreshes_stop_at_the_turns_cap(self):
        client, model, path = self.start(settings={'keep_warm': 1})
        def run(bot, cap):
            client.request('create', bot=bot, workspace=str(path), effort='low')
            turn = client.request('submit', bot=bot, request_id='w', prompt='shell:sleep 2.5',
                                  budget_tokens=cap)['result']['turn']
            end = client.finished(turn)['data']
            requests = []
            while not model.requests.empty():
                requests.append(model.requests.get())
            usage = [m['data'] for m in client.saved
                     if m.get('event') == 'usage' and m.get('bot') == bot]
            return end, sum(r['max_tokens'] == 0 for r in requests), usage
        # The call spends the cap: no call follows it, so nothing is refreshed.
        end, warms, usage = run('Bob', 1)
        self.assertEqual((end['error'], warms), ('turn_budget_exhausted', 0))
        call = usage[0]['input_tokens'] + usage[0]['output_tokens']
        # Room for one refresh: the second would bill past the cap.
        end, warms, _ = run('Ann', call + 9)
        self.assertEqual((end['error'], warms), ('turn_budget_exhausted', 1))

    def test_a_long_reply_keeps_its_own_prompt_cache_warm(self):
        client, model, path = self.start(settings={'keep_warm': 1})
        model.generate_delay = 2.5
        client.request('create', bot='Bob', workspace=str(path), effort='low')
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
        client, model, path = self.start(settings={'keep_warm': 1})
        model.generate_delay = 1.5
        model.warm_delay = 1
        client.request('create', bot='Bob', workspace=str(path), effort='low')
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

    def test_the_last_replys_refresh_counts_before_a_steer_joins_the_turn(self):
        client, model, path = self.start(settings={'keep_warm': 1})
        model.generate_delay = 1.5
        model.warm_delay = 1
        # The call bills 14 tokens (5 + 2 cached in, 7 out) and its refresh 9.
        client.request('create', bot='Bob', workspace=str(path), effort='low', budget_tokens=20)
        turn = client.request('submit', bot='Bob', request_id='s1', prompt='long')['result']['turn']
        model.requests.get(timeout=5)
        steer = client.request('submit', bot='Bob', request_id='s2', prompt='more', delivery='steer')
        # The refresh still in flight when the reply ends is answered and
        # counted before the steer could join, and leaves the budget no
        # call for it: the turn completes without it, rather than failing
        # with it unheard, and the steer, run on its own, is refused.
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertEqual(client.finished(steer['result']['turn'])['data']['error'], 'budget_exhausted')
        self.assertEqual(model.requests.get(timeout=1)['max_tokens'], 0)
        self.assertTrue(model.requests.empty())
        self.assertEqual(client.request('inspect', bot='Bob')['result']['tokens_used'], 23)

    def test_a_call_waiting_to_be_sent_is_refreshed_only_from_its_send(self):
        # One request may start at a time and Ann's waits for its headers,
        # so Bob's call waits to be sent; its cache exists only from then.
        client, model, path = self.start(extra=('--max-connecting', '1'), settings={'keep_warm': 1})
        model.hold_delay = 2.5
        model.generate_delay = 1.5
        model.arrivals = []
        for bot in ('Ann', 'Bob'):
            client.request('create', bot=bot, workspace=str(path), effort='low')
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
        client, model, path = self.start(settings={'keep_warm': 1})
        model.refuse_warm = True
        client.request('create', bot='Bob', workspace=str(path), effort='low')
        turn = client.request('submit', bot='Bob', request_id='w1', prompt='shell:sleep 2.5')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        failed = [m for m in client.saved if m.get('event') == 'keep_warm_failed']
        self.assertEqual([(m['turn'], m['error'], m['durable']) for m in failed],
                         [(turn, 'provider_http_400', False)])
        warms = [r for r in iter(lambda: None if model.requests.empty() else model.requests.get(), None)
                 if r['max_tokens'] == 0]
        self.assertEqual(len(warms), 1)

    def test_a_refresh_sent_before_the_tool_ends_is_still_recorded(self):
        client, model, path = self.start(settings={'keep_warm': 1})
        model.warm_delay = 1
        client.request('create', bot='Bob', workspace=str(path), effort='low')
        turn = client.request('submit', bot='Bob', request_id='w1', prompt='shell:sleep 1.5')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        usage = [m['data'] for m in client.saved if m.get('event') == 'usage']
        self.assertEqual([u.get('purpose') for u in usage], [None, 'keep_warm', None])

    def test_an_interrupt_still_records_a_refresh_already_sent(self):
        client, model, path = self.start(settings={'keep_warm': 1})
        model.warm_delay = 1.5
        client.request('create', bot='Bob', workspace=str(path), effort='low')
        turn = client.request('submit', bot='Bob', request_id='i1', prompt='shell:sleep 10')['result']['turn']
        call = model.requests.get(timeout=5)
        self.assertEqual(model.requests.get(timeout=5)['max_tokens'], 0)  # the refresh is in flight
        client.request('interrupt', bot='Bob', turn=turn)
        self.assertEqual(client.finished(turn)['data']['status'], 'interrupted')
        usage = [m['data'] for m in client.saved if m.get('event') == 'usage']
        self.assertEqual([u.get('purpose') for u in usage], [None, 'keep_warm'])
        self.assertEqual(call['max_tokens'] > 0, True)

    def test_an_hour_long_cache_is_marked_priced_apart_and_not_refreshed(self):
        client, model, path = self.start(settings={'keep_warm': 1, 'cache_ttl': '1h'})
        model.cache_control = {'type': 'ephemeral', 'ttl': '1h'}
        # A report without the per-lifetime split: every write is an hour's.
        model.start_usage = {'cache_creation_input_tokens': 3}
        client.request('create', bot='Bob', workspace=str(path), effort='low')
        turn = client.request('submit', bot='Bob', request_id='h1', prompt='shell:sleep 1.5')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertEqual([r['max_tokens'] > 0 for r in (model.requests.get(timeout=1),
                                                         model.requests.get(timeout=1))], [True, True])
        self.assertTrue(model.requests.empty())
        usage = [m['data'] for m in client.saved if m.get('event') == 'usage']
        self.assertEqual([(u['cache_write_tokens'], u['cache_write_1h_tokens']) for u in usage], [(3, 3)] * 2)

    def test_configure_moves_a_bot_to_the_hour_long_cache(self):
        client, model, path = self.start()
        client.request('create', bot='Bob', workspace=str(path), effort='low')
        moved = client.request('configure', bot='Bob', settings={'cache_ttl': '1h'})['result']
        self.assertEqual(moved['changed'], ['cache_ttl'])
        self.assertEqual(client.request('configure', bot='Bob', settings={'cache_ttl': '1h'})['result']['changed'], [])
        self.assertEqual(client.request('inspect', bot='Bob')['result']['settings']['cache_ttl'], '1h')
        # Its next call marks the cache for an hour.
        model.cache_control = {'type': 'ephemeral', 'ttl': '1h'}
        turn = client.request('submit', bot='Bob', request_id='c1', prompt='hello')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')

    def test_configure_serves_a_bot_in_fast_mode_and_back(self):
        client, model, path = self.start()
        client.request('create', bot='Bob', workspace=str(path), effort='low')
        self.assertEqual(client.request('inspect', bot='Bob')['result']['settings']['speed'], 'standard')
        # The Claude API serves fast alone; the refusal names what it serves.
        refused = client.request('configure', bot='Bob', settings={'speed': 'ultrafast'})
        self.assertEqual(refused['error'], 'invalid_speed')
        self.assertEqual(refused['speeds'], ['fast'])
        self.assertEqual(client.request('configure', bot='Bob', settings={'speed': 'fast'})['result']['changed'], ['speed'])
        self.assertEqual(client.request('configure', bot='Bob', settings={'speed': 'fast'})['result']['changed'], [])
        turn = client.request('submit', bot='Bob', request_id='f1', prompt='hello')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertEqual(model.requests.get(timeout=5)['speed'], 'fast')
        # Back to standard, its calls name no speed.
        self.assertEqual(client.request('configure', bot='Bob', settings={'speed': 'standard'})['result']['changed'], ['speed'])
        turn = client.request('submit', bot='Bob', request_id='f2', prompt='hello')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertNotIn('speed', model.requests.get(timeout=5))
        self.assertEqual(client.ready['providers']['anthropic']['speeds'], ['fast'])

    def test_a_short_tool_call_sends_no_refresh(self):
        client, model, path = self.start()
        client.request('create', bot='Bob', workspace=str(path), effort='low')
        turn = client.request('submit', bot='Bob', request_id='w1', prompt='shell:true')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertEqual([r['max_tokens'] > 0 for r in (model.requests.get(timeout=1),
                                                         model.requests.get(timeout=1))], [True, True])
        self.assertTrue(model.requests.empty())

    def test_a_server_side_fallback_answer_replays_without_the_declined_attempt(self):
        # No request asks for one now, but a history may hold one.
        client, model, path = self.start()
        client.request('create', bot='Bob', workspace=str(path), effort='low')
        turn = client.request('submit', bot='Bob', request_id='f1', prompt='fallback:kept')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        fallback = [m for m in client.saved if m.get('event') == 'model_fallback']
        self.assertEqual([(m['from'], m['to']) for m in fallback], [('synthetic-claude', 'synthetic-fallback')])
        # Both attempts produced output, so both are billed, each at its model,
        # and the answer the turn keeps came from the model fallen back to.
        usage = [m['data'] for m in client.saved if m.get('event') == 'usage']
        # Cache writes are recorded with the attempt that made them.
        usage[0].pop('sent_ms')
        usage[0].pop('context_window')
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

    def test_a_refused_request_alone_goes_to_the_fallback_model(self):
        client, model, path = self.start()
        # The fallback's pool parks the turn once: it resumes on the fallback.
        model.limit_fallback = 1
        client.request('create', bot='Bob', workspace=str(path), effort='low',
                       fallback='anthropic/synthetic-claude-2')
        turn = client.request('submit', bot='Bob', request_id='f1', prompt='refuse:once')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        refused, limited, answered = (model.requests.get(timeout=1) for _ in range(3))
        self.assertEqual((limited['model'], limited['fallback_credit_token']), ('synthetic-claude-2', 'credit-1'))
        # The same body on the fallback model, with the refusal's credit.
        self.assertEqual((refused['model'], answered['model']), ('synthetic-claude', 'synthetic-claude-2'))
        self.assertNotIn('fallback_credit_token', refused)
        self.assertEqual(answered['fallback_credit_token'], 'credit-1')
        self.assertEqual(answered['messages'], refused['messages'])
        retried = [m['data'] for m in client.saved if m.get('event') == 'retry']
        self.assertEqual([(r['error'], r.get('fallback'), r['parked']) for r in retried],
                         [('provider_refusal', 'anthropic/synthetic-claude-2', False),
                          ('provider_http_429', None, True)])
        self.assertEqual(retried[0]['detail'], 'category cyber')
        fallback = [m for m in client.saved if m.get('event') == 'model_fallback']
        self.assertEqual([(m['from'], m['to']) for m in fallback], [('synthetic-claude', 'synthetic-claude-2')])
        usage = [m['data'] for m in client.saved if m.get('event') == 'usage']
        self.assertEqual(usage[-1]['served_model'], 'synthetic-claude-2-20260926')
        # The next request goes back to the bot's own model, with the
        # fallback's answer in its history and the declined partial not.
        turn = client.request('submit', bot='Bob', request_id='f2', prompt='hello')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        after = model.requests.get(timeout=1)
        self.assertEqual(after['model'], 'synthetic-claude')
        self.assertNotIn('fallback_credit_token', after)
        self.assertEqual([b.get('text') for b in after['messages'][1]['content'] if b['type'] == 'text'],
                         ['reply:refuse:once'])
        self.assertTrue(model.requests.empty())

    def test_a_refusal_the_fallback_repeats_fails_the_turn(self):
        client, model, path = self.start()
        client.request('create', bot='Bob', workspace=str(path), fallback='anthropic/synthetic-claude-2')
        client.request('create', bot='Solo', workspace=str(path))
        turn = client.request('submit', bot='Bob', request_id='f1', prompt='refuse:all')['result']['turn']
        finished = client.finished(turn)['data']
        self.assertEqual((finished['error'], finished['detail']), ('provider_refusal', 'category cyber'))
        self.assertEqual([model.requests.get(timeout=1)['model'] for _ in range(2)],
                         ['synthetic-claude', 'synthetic-claude-2'])
        # With no fallback, a refusal ends the turn at once.
        turn = client.request('submit', bot='Solo', request_id='s1', prompt='refuse:all')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['error'], 'provider_refusal')
        self.assertEqual(model.requests.get(timeout=1)['model'], 'synthetic-claude')
        self.assertTrue(model.requests.empty())
        # A fallback is a known provider's model in the bot's family.
        self.assertEqual(client.request('create', bot='Bad', workspace=str(path),
                                        fallback='openai/x')['error'], 'provider_unavailable')

    def test_configure_takes_every_setting_create_takes(self):
        client, model, path = self.start()
        client.request('create', bot='Bob', workspace=str(path), effort='low')
        configure = lambda **fields: client.request('configure', bot='Bob', **fields)
        inspect = lambda: client.request('inspect', bot='Bob')['result']
        # A fallback given later answers the next declined request.
        self.assertEqual(configure(fallback='anthropic/synthetic-claude-2')['result']['changed'], ['fallback'])
        turn = client.request('submit', bot='Bob', request_id='f1', prompt='refuse:all')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['error'], 'provider_refusal')
        self.assertEqual([model.requests.get(timeout=1)['model'] for _ in range(2)],
                         ['synthetic-claude', 'synthetic-claude-2'])
        self.assertEqual(configure(fallback=None)['result']['changed'], ['fallback'])
        self.assertIsNone(inspect().get('fallback'))
        self.assertEqual(configure(fallback='openai/x')['error'], 'provider_unavailable')
        # Compaction, as create takes it, and null takes it away.
        changed = configure(compaction_instructions='Summarize.',
                            compaction_model='anthropic/synthetic-claude-2')['result']['changed']
        self.assertEqual(changed, ['compaction_instructions', 'compaction_model'])
        self.assertEqual(inspect()['compaction_model'], 'anthropic/synthetic-claude-2')
        self.assertEqual(configure(compaction_model=None)['result']['changed'], ['compaction_model'])
        self.assertEqual(configure(compaction_instructions='x' * 65537)['error'], 'instructions_limit')
        # Settings by create's names: each given replaces, null resets, the rest stay.
        changed = configure(settings={'context_items': 8, 'keep_turns': 3, 'max_output_tokens': 4096})
        self.assertEqual(changed['result']['changed'], ['context_items', 'keep_turns', 'max_output_tokens'])
        settings = inspect()['settings']
        self.assertEqual((settings['context_items'], settings['keep_turns'], settings['max_output_tokens']),
                         (8, 3, 4096))
        self.assertEqual(configure(settings={'keep_turns': None})['result']['changed'], ['keep_turns'])
        settings = inspect()['settings']
        self.assertEqual((settings['context_items'], settings.get('keep_turns')), (8, None))
        self.assertEqual(configure(settings={'context_items': 8})['result']['changed'], [])
        # Create's checks, before anything is written.
        for refused in ({'colour': 'red'}, {'colour': None}, {'compact_keep': 99}, {'context_items': 1}):
            with self.subTest(settings=refused):
                self.assertEqual(configure(settings=refused, fallback='anthropic/synthetic-claude-2')['error'],
                                 'invalid_setting')
        self.assertIsNone(inspect().get('fallback'))
        # Its next turn runs with them.
        turn = client.request('submit', bot='Bob', request_id='f2', prompt='hello')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertEqual(model.requests.get(timeout=5)['max_tokens'], 4096)
        # A writable folder gone since fails only a change to the sandbox.
        if shutil.which('bwrap') or sys.platform == 'darwin':
            gone = Path(tempfile.mkdtemp(dir=path))
            configure(settings={'sandbox': 'workspace-write', 'writable': [str(gone)]})
            gone.rmdir()
            self.assertEqual(configure(settings={'keep_turns': 2})['result']['changed'], ['keep_turns'])
            self.assertEqual(configure(settings={'network': True})['error'], 'path_not_found')

    def test_what_a_declined_request_added_goes_on_as_a_note(self):
        client, model, path = self.start()
        client.request('create', bot='Bob', workspace=str(path), effort='low')
        why = "the provider's safety classifier declined the request that carried it (category: cyber)"
        # Declined at its first request: the prompt is what it added.
        words = 'refuse:all ' + 'word ' * 40
        turn = client.request('submit', bot='Bob', request_id='d1', prompt=words)['result']['turn']
        finished = client.finished(turn)['data']
        self.assertEqual(finished['error'], 'provider_refusal')
        turn = client.request('submit', bot='Bob', request_id='d2', prompt='hello')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        model.requests.get(timeout=1)
        after = model.requests.get(timeout=1)
        self.assertEqual(after['messages'][0]['content'],
                         [{'type': 'text', 'text': f'[A message here is left out: {why}.]'}])
        self.assertEqual(after['messages'][1]['content'], [{'type': 'text', 'text': 'hello'}])
        # Declined after a tool ran: only its result goes as a note; the
        # prompt and the call the model made stay.
        read = 'tool:refuse:all ' + 'line ' * 40
        turn = client.request('submit', bot='Bob', request_id='d3', prompt=read)['result']['turn']
        self.assertEqual(client.finished(turn)['data']['error'], 'provider_refusal')
        turn = client.request('submit', bot='Bob', request_id='d4', prompt='again')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        while not model.requests.empty():
            last = model.requests.get(timeout=1)
        sent = [m['content'] for m in last['messages']]
        self.assertEqual(sent[-4][0]['text'], read)
        self.assertEqual(sent[-3][1]['input'], {'text': read[5:]})
        self.assertEqual(sent[-2], [{'type': 'tool_result', 'tool_use_id': 'toolu_1',
                                     'content': f'[This tool result is left out: {why}.]'}])
        self.assertEqual(sent[-1], [{'type': 'text', 'text': 'again'}])
        # The history reads give the originals back; only the model gets notes.
        with closing(sqlite3.connect(path / 'state.sqlite')) as db:
            kept = [row[0] for row in db.execute('SELECT node FROM declined ORDER BY node')]
        items = [node_item(client, 'Bob', node)['result'] for node in kept]
        self.assertEqual(items[0]['content'][0]['text'], words)
        self.assertEqual(items[1]['content'][0]['content'], read[5:])
        # The person's words stay in the record, and a retry still matches them.
        turns = client.request('turns', bot='Bob', after=0, limit=8)['result']['turns']
        self.assertEqual(turns[0]['prompt_preview'], words[:200])
        again = client.request('submit', bot='Bob', request_id='d1', prompt=words)
        self.assertEqual(again['result']['turn'], turns[0]['turn'], again)

    def test_a_fork_taken_while_a_request_is_out_keeps_what_it_declined(self):
        client, model, path = self.start()
        client.request('create', bot='Bob', workspace=str(path), effort='low')
        # The fork takes the prompt while the request is out; the refusal
        # that follows is Bob's, not the fork's.
        model.generate_delay = 1
        words = 'refuse:all ' + 'word ' * 40
        turn = client.request('submit', bot='Bob', request_id='d1', prompt=words)['result']['turn']
        model.requests.get(timeout=5)
        self.assertIn('result', client.request('fork', source='Bob', bot='Branch'))
        self.assertEqual(client.finished(turn)['data']['error'], 'provider_refusal')
        turn = client.request('submit', bot='Branch', request_id='b1', prompt='hello')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        sent = model.requests.get(timeout=1)
        self.assertEqual(sent['messages'][0]['content'], [{'type': 'text', 'text': words}])

    def test_a_hosted_search_is_declared_kept_and_its_paused_turn_resumed(self):
        client, model, path = self.start()
        self.assertTrue(client.ready['providers']['anthropic']['web_search'])
        client.request('create', bot='Bob', workspace=str(path), effort='low',
                       tools=['echo', 'shell', 'web_search'])
        turn = client.request('submit', bot='Bob', request_id='s1', prompt='search:rust')['result']['turn']
        finished = client.finished(turn)
        self.assertEqual(finished['data']['status'], 'completed')
        first, second = model.requests.get(timeout=1), model.requests.get(timeout=1)
        self.assertTrue(model.requests.empty())
        self.assertEqual([t.get('name') for t in first['tools']], ['echo', 'shell', 'web_search'])
        self.assertTrue(first['tools'][2]['type'].startswith('web_search_'))
        # The pause resends the history as it stood, with no new message:
        # the search and its results as the server sent them.
        self.assertEqual(len(second['messages']), 2)
        kept = second['messages'][1]
        self.assertEqual(kept['role'], 'assistant')
        self.assertEqual([b['type'] for b in kept['content']],
                         ['thinking', 'server_tool_use', 'web_search_tool_result', 'text'])
        self.assertEqual(kept['content'][1]['input'], {'query': 'rust'})
        self.assertEqual(kept['content'][2]['content'][0]['encrypted_content'], 'opaque')
        # Nothing ran here, and the search needed no approval.
        events = client.request('events', bot='Bob', after=0, limit=64)['result']['events']
        self.assertFalse([e for e in events if e['event'].startswith('tool_')])

    def test_an_empty_reply_ends_a_turn_after_a_tool_call_and_after_a_prompt(self):
        client, model, path = self.start()
        client.request('create', bot='Bob', workspace=str(path))
        turn = client.request('submit', bot='Bob', request_id='r1', prompt='tool:silent')['result']['turn']
        finished = client.finished(turn)
        self.assertEqual(finished['data']['status'], 'completed')
        self.assertEqual(len([m for m in client.saved if m.get('event') == 'usage']), 2)
        # The history it left takes the next prompt.
        turn = client.request('submit', bot='Bob', request_id='r2', prompt='hello')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        # A reply of nothing to a prompt is the bot choosing to say nothing.
        turn = client.request('submit', bot='Bob', request_id='r3', prompt='silent')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        waited = client.request('wait', handles=[f'turn:Bob/{turn}'], timeout_ms=0)['result']
        self.assertEqual(waited['results'][f'turn:Bob/{turn}']['text'], '')
        # And the next prompt still goes.
        turn = client.request('submit', bot='Bob', request_id='r4', prompt='hello')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')

    def test_an_empty_reply_after_a_call_that_parked_ends_the_turn(self):
        # The call waits on the person's verdict, so the turn parks and
        # resumes; it had replied before the park.
        client, model, path = self.start(settings={'approval_hold_ms': 0})
        client.request('create', bot='Bob', workspace=str(path), approve=['echo'], approver='manual')
        turn = client.request('submit', bot='Bob', request_id='r1', prompt='tool:silent')['result']['turn']
        client.receive(lambda m: m.get('event') == 'turn_waiting' and m.get('turn') == turn
                       and m['data'].get('approval'))
        client.request('answer', bot='Bob', turn=turn, call_id='toolu_1', request=1,
                       decision='allow', by='test')
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')

    def test_a_lone_overload_mid_stream_backs_off_its_own_call(self):
        client, model, path = self.start()
        model.overloads = 1
        client.request('create', bot='Bob', workspace=str(path))
        turn = client.request('submit', bot='Bob', request_id='r1', prompt='hello')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        retried = [m['data'] for m in client.saved if m.get('event') == 'retry']
        self.assertEqual([r['error'] for r in retried], ['provider_http_529'])
        self.assertEqual(retried[0]['detail'], 'Overloaded')
        # One overload leaves the pool open for the other bots on the model:
        # this call retries after half a second, as the providers' SDKs do.
        self.assertFalse(retried[0]['parked'])
        self.assertTrue(400 <= retried[0]['delay_ms'] <= 600, retried[0])

    def test_messages_family_round_trips_thinking_tools_and_usage(self):
        client, model, path = self.start()
        client.request('create', bot='Bob', workspace=str(path), effort='low',
                       fallback='anthropic/synthetic-claude-2')
        before = time.time() * 1000
        turn = client.request('submit', bot='Bob', request_id='r1', prompt='tool:shared')['result']['turn']
        finished = client.finished(turn)
        after = time.time() * 1000
        self.assertEqual(finished['data']['status'], 'completed')
        deltas = [m for m in client.saved if m.get('event') in ('text_delta', 'thinking_delta')]
        self.assertEqual([d['event'] for d in deltas][:2], ['thinking_delta', 'thinking_delta'])
        self.assertEqual(''.join(d['text'] for d in deltas if d['event'] == 'text_delta'), 'echo:shared')
        # Each call's message is an item of its own, numbered on through the turn.
        items = [d['item'] for d in deltas]
        self.assertEqual((items, len(set(items))), (sorted(items), 2))
        usage = [m for m in client.saved if m.get('event') == 'usage']
        self.assertEqual(len(usage), 2)
        # Each call records when it was sent, in epoch milliseconds.
        sent = [u['data'].pop('sent_ms') for u in usage]
        self.assertTrue(before - 1 <= sent[0] <= sent[1] <= after + 1, (before, sent, after))
        # The call names the dated model that answered, not only the one asked,
        # and the model's context window, for a client to say how full it is.
        self.assertEqual(usage[0]['data'], {'input_tokens': 7, 'output_tokens': 7, 'cached_input_tokens': 2,
                                            'served_model': 'synthetic-claude-20260926',
                                            'context_window': 200000})
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
        state = client.request('inspect', bot='Bob')['result']
        self.assertEqual((state['provider'], state['family'], state['model'], state['effort']),
                         ('anthropic', 'anthropic', 'synthetic-claude', 'low'))
        self.assertEqual(client.request('create', bot='Bad', workspace=str(path), effort='extreme')['error'],
                         'invalid_effort')
        self.assertEqual(client.request('create', bot='Bad', workspace=str(path), model='openai/x')['error'],
                         'provider_unavailable')
        client.request('create', bot='Capped', workspace=str(path), budget_tokens=10)
        capped = client.request('submit', bot='Capped', request_id='cap', prompt='incomplete')['result']['turn']
        self.assertEqual(client.finished(capped)['data']['error'], 'provider_incomplete')
        self.assertEqual(client.request('inspect', bot='Capped')['result']['tokens_used'], 14)  # 5 + 2 cached in, 7 out
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

    def client(self, tools="echo", extra=(), settings=None):
        # Started as the CLI starts a daemon, protecting what it names.
        client = Client(self.binary, self.path / 'state.sqlite', self.url, tools,
                        extra=(*PROTECTING, *extra), settings=settings)
        self.addCleanup(client.close)
        return client



@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class RuntimeTests(ModelFixture):
    def test_detached_shell_releases_its_daemon_slot_after_exit(self):
        client = self.client('shell', extra=('--max-lasting', '1'))
        client.request('create', bot='Bob', workspace=str(self.path), tools=['shell'])

        def result(prompt, request_id):
            turn = client.request('submit', bot='Bob', request_id=request_id,
                                  prompt=prompt)['result']['turn']
            self.assertEqual(client.finished(turn)['data']['status'], 'completed')
            events = client.request('events', bot='Bob', after=0, limit=64)['result']['events']
            completed = next(e for e in events if e['turn'] == turn and e['event'] == 'tool_completed')
            output = node_item(client, 'Bob', completed['data']['node'])['result']['output']
            return json.loads(output)

        first = result('detach:exec sleep 30', 'first')
        self.assertTrue(first['detached'])
        pid = first['pid']
        try:
            self.assertEqual(result('detach:true', 'full')['error'], 'lasting_limit')
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
        # The running turn parks, as a pull parks it, for the next start.
        parked = client.receive(lambda e: e.get('event') == 'turn_paced' and e.get('turn') == turn)
        self.assertEqual(parked['data'], {'resume_at_ms': None, 'stopped': True})
        # The wait is answered as its deadline would answer it: still pending,
        # the turn shown parked.
        waited = client.receive(lambda e: e.get('id') == 'waiter')['result']
        self.assertEqual(waited['pending'], [handle])
        self.assertEqual((waited['results'][handle]['status'], waited['results'][handle]['waiting_on']),
                         ('paced', {'resume_at_ms': None, 'stopped': True}))
        self.assertEqual(client.process.wait(timeout=2), 0)
        restarted = self.client()
        # It goes on from where it stopped, and the time down is not pacing.
        self.model.requests.get(timeout=3)
        view = restarted.request('wait', handles=[handle], timeout_ms=0)['result']['results'][handle]
        self.assertEqual((view['status'], view['paced_ms']), ('running', 0))
        replay = restarted.request('events', bot='Bob', after=0, limit=256)['result']['events']
        self.assertFalse(any(e['event'] == 'turn_finished' for e in replay))

    def test_a_drain_starts_no_call_and_parks_the_turn_for_the_next_start(self):
        # The model is answering when the drain starts. The calls it returns
        # do not start: the turn parks long before the grace ends, and the
        # next start continues it with those calls answered as not run.
        gate = threading.Event()
        self.model.request_gates = queue.Queue()
        self.model.request_gates.put(gate)
        self.model.call_script = [[('shell', {'command': 'echo x > ran'}), ('shell', {'command': 'echo y'})]]
        client = self.client('shell')
        self.addCleanup(lambda: client.close(kill=True))
        client.request('create', bot='Bob', workspace=str(self.path), tools=['shell'])
        turn = client.request('submit', bot='Bob', request_id='drain', prompt='script')['result']['turn']
        self.model.requests.get(timeout=3)
        started = time.monotonic()
        self.assertTrue(client.request('shutdown', grace_ms=60_000)['result']['shutting_down'])
        gate.set()
        parked = client.receive(lambda e: e.get('event') == 'turn_paced' and e.get('turn') == turn)
        self.assertEqual(parked['data'], {'resume_at_ms': None, 'stopped': True})
        self.assertEqual(client.process.wait(timeout=10), 0)
        self.assertLess(time.monotonic() - started, 10)
        self.assertFalse((self.path / 'ran').exists())
        restarted = self.client('shell')
        self.addCleanup(lambda: restarted.close(kill=True))
        handle = f'turn:Bob/{turn}'
        done = restarted.request('wait', handles=[handle], timeout_ms=10_000)['result']['results'][handle]
        self.assertEqual((done['status'], done['paced_ms']), ('completed', 0))
        request = self.model.requests.get(timeout=3)
        outputs = {i['call_id']: json.loads(i['output']) for i in request['input']
                   if i.get('type') == 'function_call_output'}
        self.assertEqual(set(outputs), {'script-0', 'script-0.1'})
        for output in outputs.values():
            self.assertEqual(output['error'], 'daemon_shutdown')
            self.assertIn('did not run', output['detail'])
        self.assertFalse((self.path / 'ran').exists())

    def test_a_drain_lets_a_running_command_finish_then_parks_before_the_next_call(self):
        marker = self.path / 'ran'
        self.model.slow_command = f'sleep 1; echo x >> {marker}; echo finished'
        client = self.client('echo,shell')
        self.addCleanup(lambda: client.close(kill=True))
        client.request('create', bot='Alice', workspace=str(self.path))
        turn = client.request('submit', bot='Alice', request_id='a', prompt='sayslow:the review')['result']['turn']
        client.receive(lambda m: m.get('event') == 'tool_started' and m.get('turn') == turn)
        self.model.requests.get(timeout=3)
        self.assertTrue(client.request('shutdown', grace_ms=60_000)['result']['shutting_down'])
        # The command runs to its result, which is kept; then the turn parks
        # rather than asking the model again.
        done = client.receive(lambda m: m.get('event') == 'tool_completed' and m.get('turn') == turn)
        self.assertNotIn('cancelled', done['data'])
        parked = client.receive(lambda e: e.get('event') == 'turn_paced' and e.get('turn') == turn)
        self.assertEqual(parked['data'], {'resume_at_ms': None, 'stopped': True})
        self.assertEqual(client.process.wait(timeout=10), 0)
        self.assertTrue(self.model.requests.empty())
        restarted = self.client('echo,shell')
        self.addCleanup(lambda: restarted.close(kill=True))
        self.assertEqual(restarted.finished(turn)['data']['status'], 'completed')
        request = self.model.requests.get(timeout=3)
        output = json.loads(next(i for i in request['input'] if i.get('type') == 'function_call_output'
                                 and i['call_id'] == 'slow-1')['output'])
        self.assertIn('finished', json.dumps(output))
        self.assertEqual(marker.read_text(), 'x\n')

    def test_a_drain_sends_no_retry_of_a_failed_model_call(self):
        # The model call failed and waits 0.7 s for its retry when the drain
        # begins: no retry is sent, and the turn parks for the next start.
        client = self.client()
        self.addCleanup(lambda: client.close(kill=True))
        client.request('create', bot='Bob', workspace=str(self.path))
        turn = client.request('submit', bot='Bob', request_id='r', prompt='busy:1')['result']['turn']
        client.receive(lambda e: e.get('event') == 'retry' and e.get('turn') == turn)
        self.assertTrue(client.request('shutdown', grace_ms=60_000)['result']['shutting_down'])
        parked = client.receive(lambda e: e.get('event') == 'turn_paced' and e.get('turn') == turn)
        self.assertEqual(parked['data'], {'resume_at_ms': None, 'stopped': True})
        self.assertEqual(client.process.wait(timeout=10), 0)
        self.assertEqual(sum(1 for e in client.saved if e.get('event') == 'turn_finished'), 0)
        restarted = self.client()
        self.addCleanup(lambda: restarted.close(kill=True))
        self.assertEqual(restarted.finished(turn)['data']['status'], 'completed')

    def test_a_drain_starts_no_more_of_a_wide_batch_and_keeps_what_ran(self):
        # Five commands run together, four at a time: the fifth would start
        # as the first finishes, but the drain began, so it does not run.
        slow = f'sleep 1; echo x >> {self.path / "ran"}'
        self.model.call_script = [[('shell', {'command': slow}) for _ in range(4)]
                                  + [('shell', {'command': f'echo y > {self.path / "late"}'})]]
        client = self.client('shell')
        self.addCleanup(lambda: client.close(kill=True))
        client.request('create', bot='Bob', workspace=str(self.path), tools=['shell'])
        turn = client.request('submit', bot='Bob', request_id='wide', prompt='script')['result']['turn']
        client.receive(lambda m: m.get('event') == 'tool_started' and m.get('turn') == turn)
        self.model.requests.get(timeout=3)
        self.assertTrue(client.request('shutdown', grace_ms=60_000)['result']['shutting_down'])
        parked = client.receive(lambda e: e.get('event') == 'turn_paced' and e.get('turn') == turn)
        self.assertEqual(parked['data'], {'resume_at_ms': None, 'stopped': True})
        self.assertEqual(client.process.wait(timeout=10), 0)
        self.assertEqual((self.path / 'ran').read_text(), 'x\n' * 4)
        self.assertFalse((self.path / 'late').exists())
        restarted = self.client('shell')
        self.addCleanup(lambda: restarted.close(kill=True))
        self.assertEqual(restarted.finished(turn)['data']['status'], 'completed')
        request = self.model.requests.get(timeout=3)
        outputs = {i['call_id']: json.loads(i['output']) for i in request['input']
                   if i.get('type') == 'function_call_output'}
        self.assertEqual(len(outputs), 5)
        stopped = [o for o in outputs.values() if o.get('error') == 'daemon_shutdown']
        self.assertEqual(len(stopped), 1)
        self.assertIn('did not run', stopped[0]['detail'])
        self.assertFalse((self.path / 'late').exists())

    def test_a_pause_stops_every_agent_until_resume_across_restarts(self):
        self.model.release_headers = threading.Event()
        self.model.all_streaming = self.model.release_headers
        client = self.client()
        self.addCleanup(lambda: client.close(kill=True))
        client.request('create', bot='Bob', workspace=str(self.path))
        client.request('create', bot='Alice', workspace=str(self.path))
        turn = client.request('submit', bot='Bob', request_id='running', prompt='gate')['result']['turn']
        self.model.requests.get(timeout=3)
        self.assertEqual(client.request('pause', reason=' ')['error'], 'usage')
        # Who pulled it is told to every session, so it is bounded.
        self.assertEqual(client.request('pause', reason='why', by='x' * 129)['error'], 'invalid_by')
        self.assertIsNone(client.request('stats')['result']['paused'])
        paused = client.request('pause', reason='the migration is deleting rows', by='calc.a')['result']
        self.assertEqual((paused['changed'], paused['parked']), (True, 1))
        self.assertEqual(paused['pause']['reason'], 'the migration is deleting rows')
        self.assertEqual(paused['pause']['by'], 'calc.a')
        # It names whose running turns it parked, so one can be stopped first.
        self.assertEqual(paused['pause']['parked'], {'bots': ['Bob'], 'total': 1})
        heard = client.receive(lambda e: e.get('event') == 'paused')
        self.assertEqual(heard['pause'], paused['pause'])
        # The running turn parks now, to go on when the pause lifts.
        parked = client.receive(lambda e: e.get('event') == 'turn_paced' and e.get('turn') == turn)
        self.assertEqual(parked['data'], {'resume_at_ms': None, 'paused': True})
        running = f'turn:Bob/{turn}'
        view = client.request('wait', handles=[running], timeout_ms=0)['result']['results'][running]
        self.assertEqual((view['status'], view['waiting_on']), ('paced', {'resume_at_ms': None, 'paused': True}))
        # A second pull changes nothing: the first pause holds.
        again = client.request('pause', reason='another', by='shop.b')['result']
        self.assertEqual((again['changed'], again['pause']), (False, paused['pause']))
        self.assertEqual(client.request('stats')['result']['paused'], paused['pause'])
        # Nothing starts: work that would start now is refused, and queued
        # work waits, across a restart too.
        refused = client.request('submit', bot='Alice', request_id='refused', prompt='hello')
        self.assertEqual(refused['error'], 'daemon_paused')
        held = client.request('submit', bot='Alice', request_id='held', prompt='hello', delivery='queue')['result']
        self.assertEqual(held['status'], 'ready')
        self.assertTrue(client.request('shutdown')['result']['shutting_down'])
        self.assertEqual(client.process.wait(timeout=3), 0)
        restarted = self.client()
        self.addCleanup(lambda: restarted.close(kill=True))
        self.assertEqual(restarted.request('stats')['result']['paused'], paused['pause'])
        time.sleep(.3)
        self.assertTrue(self.model.requests.empty())
        handle = f"turn:Alice/{held['turn']}"
        # Resume lifts it, says so, what waited runs, and the parked turn
        # goes on from where it stopped.
        self.model.release_headers.set()
        # Who resumed is bounded the same way, and the pause holds.
        self.assertEqual(restarted.request('resume', by='x' * 129)['error'], 'invalid_by')
        self.assertEqual(restarted.request('resume', by='')['error'], 'invalid_by')
        self.assertEqual(restarted.request('stats')['result']['paused'], paused['pause'])
        resumed = restarted.request('resume', by='shop.b')['result']
        lifted = resumed['pause']
        self.assertEqual((resumed['changed'], resumed['by'], lifted['resumed_by']), (True, 'shop.b', 'shop.b'))
        self.assertGreaterEqual(lifted['resumed_ms'], lifted['at_ms'])
        self.assertEqual({k: v for k, v in lifted.items() if not k.startswith('resumed')}, paused['pause'])
        # Every session hears who let the line go on.
        heard = restarted.receive(lambda e: e.get('event') == 'resumed')
        self.assertEqual((heard['pause'], heard['by']), (lifted, 'shop.b'))
        # The pause stays in the history, held or lifted.
        self.assertEqual(restarted.request('pauses')['result'], {'pauses': [lifted], 'more': False})
        waited = restarted.request('wait', handles=[handle, running], timeout_ms=5000)['result']
        self.assertEqual(waited['results'][handle]['status'], 'completed')
        self.assertEqual(waited['results'][running]['status'], 'completed')
        # The pause is not counted as time spent paced by a rate limit.
        self.assertLess(waited['results'][running]['paced_ms'], 300)
        self.assertEqual(restarted.request('resume')['result'], {'pause': None, 'by': None, 'changed': False})
        self.assertIsNone(restarted.request('stats')['result']['paused'])

    def test_a_turn_a_pause_parked_can_be_stopped_before_the_resume(self):
        self.model.release_headers = threading.Event()
        self.model.all_streaming = self.model.release_headers
        client = self.client()
        self.addCleanup(lambda: client.close(kill=True))
        client.request('create', bot='Bob', workspace=str(self.path))
        turn = client.request('submit', bot='Bob', request_id='culprit', prompt='gate')['result']['turn']
        self.model.requests.get(timeout=3)
        client.request('pause', reason='Bob is deleting rows', by='calc.a')
        client.receive(lambda e: e.get('event') == 'turn_paced' and e.get('turn') == turn)
        self.model.release_headers.set()
        client.request('interrupt', bot='Bob', turn=turn)
        self.assertEqual(client.finished(turn)['data']['status'], 'interrupted')
        client.request('resume')
        time.sleep(.3)
        self.assertTrue(self.model.requests.empty())

    def test_every_pause_is_kept_with_who_pulled_and_who_lifted_it(self):
        client = self.client()
        self.addCleanup(lambda: client.close(kill=True))
        self.assertEqual(client.request('pauses')['result'], {'pauses': [], 'more': False})
        for n in range(3):
            client.request('pause', reason=f'pull {n}', by='calc.a')
            client.request('resume', by='shop.b')
        held = client.request('pause', reason='pull 3')['result']['pause']
        self.assertEqual(held['parked'], {'bots': [], 'total': 0})
        first = client.request('pauses', limit=2)['result']
        self.assertEqual(([p['reason'] for p in first['pauses']], first['more']), (['pull 0', 'pull 1'], True))
        self.assertEqual({p['resumed_by'] for p in first['pauses']}, {'shop.b'})
        rest = client.request('pauses', after=first['pauses'][-1]['id'])['result']
        self.assertEqual(([p['reason'] for p in rest['pauses']], rest['more']), (['pull 2', 'pull 3'], False))
        # The one that holds says nothing of a resume yet.
        self.assertEqual(rest['pauses'][-1], held)
        newest = client.request('pauses', newest=True, limit=1)['result']
        self.assertEqual((newest['pauses'], newest['more']), ([held], True))
        older = client.request('pauses', newest=True, before=held['id'], limit=1)['result']
        self.assertEqual(older['pauses'][0]['reason'], 'pull 2')
        for page in ({'limit': 0}, {'limit': 257}, {'after': -1}, {'before': 0}):
            self.assertEqual(client.request('pauses', **page)['error'], 'invalid_pause_page')

    def test_a_pause_stops_a_running_command_and_the_turn_goes_on_without_rerunning_it(self):
        marker = self.path / 'ran'
        self.model.slow_command = f'echo x >> {marker}; sleep 30'
        client = self.client('echo,shell')
        self.addCleanup(lambda: client.close(kill=True))
        client.request('create', bot='Alice', workspace=str(self.path))
        turn = client.request('submit', bot='Alice', request_id='a', prompt='sayslow:the review')['result']['turn']
        client.receive(lambda m: m.get('event') == 'tool_started' and m.get('turn') == turn)
        deadline = time.monotonic() + 5
        while not marker.exists() and time.monotonic() < deadline:
            time.sleep(.05)
        paused = client.request('pause', reason='the loop is sending mail', by='calc.a')['result']
        self.assertEqual(paused['parked'], 1)
        # The command's call gets its result: the pause stopped it.
        done = client.receive(lambda m: m.get('event') == 'tool_completed' and m.get('turn') == turn)
        self.assertEqual(done['data']['call_id'], 'slow-1')
        parked = client.receive(lambda e: e.get('event') == 'turn_paced' and e.get('turn') == turn)
        self.assertTrue(parked['data']['paused'])
        deadline = time.monotonic() + 5
        while client.request('stats')['result']['running_processes'] and time.monotonic() < deadline:
            time.sleep(.05)
        self.assertEqual(client.request('stats')['result']['running_processes'], 0)
        while not self.model.requests.empty():
            self.model.requests.get_nowait()
        client.request('resume')
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        # The model reads why, and the command did not run again.
        request = self.model.requests.get(timeout=3)
        output = json.loads(next(i for i in request['input'] if i.get('type') == 'function_call_output'
                                 and i['call_id'] == 'slow-1')['output'])
        self.assertEqual(output['error'], 'daemon_paused')
        self.assertIn('not run again', output['detail'])
        self.assertEqual(output['pause']['reason'], 'the loop is sending mail')
        self.assertEqual(marker.read_text(), 'x\n')

    def test_a_pause_ends_the_background_commands_agents_started(self):
        self.model.background_timeout_ms = 30000
        client = self.client('shell')
        self.addCleanup(lambda: client.close(kill=True))
        client.request('create', bot='Bob', workspace=str(self.path))
        turn = client.request('submit', bot='Bob', request_id='background',
                              prompt='bg:sleep 30')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertEqual(client.request('stats')['result']['running_processes'], 1)
        client.request('pause', reason='the loop is sending mail', by='calc.a')
        deadline = time.monotonic() + 5
        while client.request('stats')['result']['running_processes'] and time.monotonic() < deadline:
            time.sleep(.05)
        self.assertEqual(client.request('stats')['result']['running_processes'], 0)

    def test_a_pause_ends_the_detached_commands_agents_started(self):
        client = self.client('shell')
        self.addCleanup(lambda: client.close(kill=True))
        client.request('create', bot='Bob', workspace=str(self.path), tools=['shell'])

        def detach(command, request_id):
            turn = client.request('submit', bot='Bob', request_id=request_id,
                                  prompt=f'detach:{command}')['result']['turn']
            self.assertEqual(client.finished(turn)['data']['status'], 'completed')
            events = client.request('events', bot='Bob', after=0, limit=64)['result']['events']
            completed = next(e for e in events if e['turn'] == turn and e['event'] == 'tool_completed')
            return json.loads(node_item(client, 'Bob', completed['data']['node'])['result']['output'])['pid']

        def alive(pid):
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                return False
            return True

        running = detach('exec sleep 30', 'running')
        # A launcher that leaves its work running in its group and exits.
        left = self.path / 'left'
        launcher = detach(f'sleep 30 & echo $! > {left}', 'launcher')
        deadline = time.monotonic() + 5
        while (alive(launcher) or not left.exists() or not left.read_text().strip()) and time.monotonic() < deadline:
            time.sleep(.05)
        child = int(left.read_text())
        for pid in (running, child):
            self.addCleanup(lambda pid=pid: subprocess.run(['kill', '-9', str(pid)], capture_output=True))
            self.assertTrue(alive(pid))
        client.request('pause', reason='the loop is sending mail', by='calc.a')
        deadline = time.monotonic() + 5
        while (alive(running) or alive(child)) and time.monotonic() < deadline:
            time.sleep(.05)
        self.assertEqual((alive(running), alive(child)), (False, False))

    @unittest.skipUnless(shutil.which('bwrap') or sys.platform == 'darwin', 'sandboxes need bubblewrap')
    def test_a_pause_ends_a_sandboxed_detached_command(self):
        client = self.client('shell')
        self.addCleanup(lambda: client.close(kill=True))
        client.request('create', bot='Bob', workspace=str(self.path), tools=['shell'],
                       settings={'sandbox': 'workspace-write'})
        ticks = self.path / 'ticks'
        turn = client.request('submit', bot='Bob', request_id='loop',
                              prompt=f'detach:while :; do echo x >> {ticks}; sleep .05; done')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        deadline = time.monotonic() + 5
        while (not ticks.exists() or ticks.stat().st_size < 4) and time.monotonic() < deadline:
            time.sleep(.05)
        self.assertGreaterEqual(ticks.stat().st_size, 4)
        client.request('pause', reason='the loop is sending mail', by='calc.a')
        time.sleep(.5)
        before = ticks.stat().st_size
        time.sleep(.5)
        # Nothing in the sandbox runs on: the loop stopped writing.
        self.assertEqual(ticks.stat().st_size, before)

    def test_a_pause_ends_a_command_a_steer_moved_to_the_background(self):
        self.model.slow_command = 'sleep 30'
        client = self.client('echo,shell,wait')
        self.addCleanup(lambda: client.close(kill=True))
        client.request('create', bot='Alice', workspace=str(self.path))
        alice = client.request('submit', bot='Alice', request_id='a', prompt='sayslow:the review')['result']
        client.receive(lambda m: m.get('event') == 'tool_started' and m.get('turn') == alice['turn'])
        client.request('submit', bot='Alice', request_id='s', prompt='steer:on', delivery='steer')
        self.assertEqual(client.finished(alice['turn'])['data']['status'], 'completed')
        self.assertEqual(client.request('stats')['result']['running_processes'], 1)
        client.request('pause', reason='the loop is sending mail', by='calc.a')
        deadline = time.monotonic() + 5
        while client.request('stats')['result']['running_processes'] and time.monotonic() < deadline:
            time.sleep(.05)
        self.assertEqual(client.request('stats')['result']['running_processes'], 0)

    def test_a_client_searches_every_bots_history_as_the_tool_does(self):
        client = self.client()
        self.addCleanup(lambda: client.close(kill=True))
        for bot, prompt in [('calc.a', 'the checkpoint stalls'), ('shop.b', 'hello')]:
            client.request('create', bot=bot, workspace=str(self.path))
            turn = client.request('submit', bot=bot, request_id=bot, prompt=prompt)['result']['turn']
            self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        found = client.request('history_search', bot='*', query='CHECKPOINT')['result']
        self.assertEqual({match['bot'] for match in found['matches']}, {'calc.a'})
        self.assertEqual(found['matches'][0]['handle'], 'turn:calc.a/1')
        one = client.request('history_search', bot='shop.b', query='checkpoint')['result']
        self.assertEqual(one['matches'], [])
        self.assertEqual(client.request('history_search', bot='*', query='')['error'], 'invalid_history_search')

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

    def test_shutdown_grace_expiry_parks_what_still_runs_for_the_next_start(self):
        client = self.client()
        self.addCleanup(lambda: client.close(kill=True))
        client.request('create', bot='Bob', workspace=str(self.path))
        turn = client.request('submit', bot='Bob', request_id='running', prompt='wait')['result']['turn']
        self.model.requests.get(timeout=3)
        self.assertTrue(client.request('shutdown', grace_ms=60_000)['result']['shutting_down'])
        # A later shutdown can only bring the deadline closer.
        started = time.monotonic()
        self.assertTrue(client.request('shutdown', grace_ms=300)['result']['shutting_down'])
        client.receive(lambda e: e.get('event') == 'turn_paced' and e.get('turn') == turn)
        self.assertGreaterEqual(time.monotonic() - started, .25)
        self.assertEqual(client.process.wait(timeout=3), 0)
        restarted = self.client()
        self.model.requests.get(timeout=3)
        handle = f'turn:Bob/{turn}'
        view = restarted.request('wait', handles=[handle], timeout_ms=0)['result']['results'][handle]
        self.assertEqual(view['status'], 'running')

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
        return [{'id': f'submit-{bot}', 'op': 'submit', 'bot': bot, 'request_id': 'held', 'prompt': 'wait', 'delivery': 'reject'}
                for bot in bots]

    def answered(self, client):
        return any('id' in message for message in [*client.saved, *list(client.queue.queue)] if message)

    def assert_shutdown_parked(self, client, turns):
        """Shutdown parked every started turn for the next start, rather than
        leaving it running for that start to end as interrupted."""
        for turn in turns:
            client.receive(lambda e: e.get('event') == 'turn_paced' and e.get('turn') == turn)
        self.assertEqual(client.process.wait(timeout=10), 0)
        with sqlite3.connect(self.path / 'state.sqlite') as db:
            self.assertEqual(db.execute("SELECT count(*) FROM turns WHERE status!='paced'").fetchone()[0], 0)
            self.assertEqual(db.execute("SELECT count(*) FROM bots WHERE status!='paced'").fetchone()[0], 0)

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
        self.assert_shutdown_parked(client, [r['result']['turn'] for r in replies[:-1]])

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
        self.assert_shutdown_parked(client, turns)

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
        self.assertEqual(client.request('inspect', bot='missing')['error'], 'bot_not_found')
        client.request('create', bot='Bob', workspace=str(self.path))
        first = client.request('submit', bot='Bob', request_id='first', prompt='héllo')['result']['turn']
        checkpoint = client.finished(first)['data']['checkpoint']
        second = client.request('submit', bot='Bob', request_id='second', prompt='second')['result']['turn']
        client.finished(second)
        before = client.request('events', bot='Bob', after=0, limit=256)['result']
        client.close(kill=True)
        client = self.client()
        self.assertEqual(client.request('inspect', bot='Bob')['result']['status'], 'completed')
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
        self.assertEqual(client.request('inspect', bot='Bob')['result']['head'], before['events'][-1]['data']['checkpoint'])

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
        output = node_item(client, 'Echo', done['data']['node'])['result']['output']
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
        self.assertEqual(client.request('inspect', bot='Both')['result']['tools'], ['shell', 'echo'])
        self.assertEqual(client.request('stats')['result']['active_turns'], 0)

    def test_a_declared_tool_is_shown_after_the_daemons_and_its_call_is_recorded_not_run(self):
        client = self.client('echo')
        react = {'name': 'react', 'description': 'React to a message.',
                 'parameters': {'type': 'object', 'properties': {'emoji': {'type': 'string'}},
                                'required': ['emoji'], 'additionalProperties': False}}
        for declare, detail in [([dict(react, name='echo')], 'own'), ([react, react], 'twice'),
                                ([dict(react, parameters={'type': 'string'})], 'object'),
                                ([dict(react, name='a b')], 'name')]:
            refused = client.request('create', bot='Bad', workspace=str(self.path), declare=declare)
            self.assertEqual(refused['error'], 'invalid_declare', detail)
        created = client.request('create', bot='Lead', workspace=str(self.path),
                                 declare=[dict(react, result='Reacted.')])['result']
        self.assertEqual([t['name'] for t in created['declared']], ['react'])
        # One fitting call, one that is not: the first gets the fixed
        # result at once, the second is refused, and neither runs a thing.
        self.model.call_script = [('react', {'emoji': '👍'}), ('react', {'emoji': 1})]
        turn = client.request('submit', bot='Lead', request_id='1', prompt='script')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        first = self.model.requests.get(timeout=3)
        self.assertEqual([t['name'] for t in first['tools']], ['echo', 'react'])
        self.assertEqual(first['tools'][1]['parameters'], react['parameters'])
        events = client.request('events', bot='Lead', after=0, limit=64)['result']['events']
        started = [e['data'] for e in events if e['event'] == 'tool_started']
        self.assertEqual([(s['name'], json.loads(s['arguments'])) for s in started],
                         [('react', {'emoji': '👍'}), ('react', {'emoji': 1})])
        done = [e for e in events if e['event'] == 'tool_completed']
        outputs = [node_item(client, 'Lead', d['data']['node'])['result']['output'] for d in done]
        self.assertEqual(outputs[0], 'Reacted.')
        self.assertEqual(json.loads(outputs[1])['error'], 'invalid_arguments')
        # Declaring again replaces them, and the next request shows the new set.
        configured = client.request('configure', bot='Lead', declare=[])['result']
        self.assertIn('declared', configured['changed'])
        self.assertNotIn('declared', client.request('inspect', bot='Lead')['result'])
        # The script counts the calls already in history, so this is its third.
        self.model.call_script = [None, None, ('react', {'emoji': '👍'})]
        turn = client.request('submit', bot='Lead', request_id='2', prompt='script')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        while not self.model.requests.empty():
            last = self.model.requests.get(timeout=3)
        self.assertEqual([t['name'] for t in last['tools']], ['echo'])
        events = client.request('events', bot='Lead', after=0, limit=128)['result']['events']
        done = [e for e in events if e['event'] == 'tool_completed'][-1]
        output = node_item(client, 'Lead', done['data']['node'])['result']['output']
        self.assertEqual(json.loads(output)['error'], 'tool_not_available')

    def test_a_fork_calls_only_its_allowed_tools_and_keeps_its_sources_request(self):
        client = self.client('echo,shell')
        client.request('create', bot='Both', workspace=str(self.path), tools=['shell', 'echo'])
        turn = client.request('submit', bot='Both', request_id='1', prompt='shell:true')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.model.requests.get(timeout=3)
        last = self.model.requests.get(timeout=3)
        # Absent inherits, [] allows none, null is refused, and a fork never widens.
        self.assertEqual(client.request('fork', source='Both', bot='Null', allow=None)['error'], 'invalid_allow')
        self.assertEqual(client.request('fork', source='Both', bot='Wide', allow=['sudo'])['error'],
                         'allow_not_in_source')
        plain = client.request('fork', source='Both', bot='Plain')['result']
        self.assertNotIn('allowed', plain)
        answer = client.request('fork', source='Both', bot='Answer', workspace=str(self.path), allow=[])['result']
        self.assertEqual((answer['tools'], answer['allowed']), (['shell', 'echo'], []))
        self.assertEqual(client.request('fork', source='Answer', bot='Wider', allow=['echo'])['error'],
                         'allow_not_in_source')
        self.assertEqual(client.request('fork', source='Answer', bot='Again')['result']['allowed'], [])
        # Repeats count once against the daemon's tool count.
        self.assertEqual(client.request('fork', source='Both', bot='Repeat', allow=['echo'] * 3)['result']['allowed'],
                         ['echo'])
        # The fork is shown what its source was shown, so its first request
        # repeats the source's last one, and its call is refused at dispatch.
        turn = client.request('submit', bot='Answer', request_id='1', prompt='shell:true')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        first = self.model.requests.get(timeout=3)
        self.assertEqual(json.dumps(first['instructions']), json.dumps(last['instructions']))
        self.assertEqual(json.dumps(first['tools']), json.dumps(last['tools']))
        self.assertEqual(json.dumps(first['input'][:len(last['input'])]), json.dumps(last['input']))
        events = client.request('events', bot='Answer', after=0, limit=64)['result']['events']
        done = [e for e in events if e['event'] == 'tool_completed'][0]
        output = node_item(client, 'Answer', done['data']['node'])['result']['output']
        self.assertEqual(json.loads(output)['error'], 'tool_not_available')
        self.assertEqual(client.request('inspect', bot='Answer')['result']['allowed'], [])

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
        self.assertEqual(client.request('inspect', bot='Bob')['result']['instructions'], 'Be brief.')

    def test_stores_open_under_any_provider_set_and_turns_check_the_family(self):
        client = self.client()
        client.request('create', bot='Bob', workspace=str(self.path))
        first = client.request('submit', bot='Bob', request_id='1', prompt='hello')['result']['turn']
        self.assertEqual(client.finished(first)['data']['status'], 'completed')
        before = client.request('inspect', bot='Bob')['result']
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
        self.assertEqual(client.request('inspect', bot='Bob')['result'], before)
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
        self.assertEqual(client.request('inspect', bot='Bob')['result'], before)
        client.close()
        # Back with the provider, the conversation continues where it was.
        client = self.client()
        later = client.request('submit', bot='Bob', request_id='2', prompt='back')['result']['turn']
        self.assertEqual(client.finished(later)['data']['status'], 'completed')
        self.assertEqual(poll(client, 'Bob', later)['result']['text'], 'reply:back')

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
        self.assertEqual(tip['head'], client.request('inspect', bot='Bob')['result']['head'])
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
        # Valid JSON of the wrong shape names its field and keeps its id.
        refused = client.receive(lambda m: 'error' in m and m.get('id') == 'copy')
        self.assertEqual(refused['error'], 'invalid_request')
        self.assertIn('instructions', refused['detail'])
        # A field name that fills the request line is elided in the
        # refusal, so the refusal still fits in one response.
        huge = 'x' * (1024 * 1024 - 200)
        client.process.stdin.write(json.dumps({'id': 'huge', 'op': 'inspect', 'bot': 'Bob', huge: 1}) + '\n')
        client.process.stdin.flush()
        refused = client.receive(lambda m: 'error' in m and m.get('id') == 'huge')
        self.assertEqual(refused['error'], 'invalid_request')
        self.assertIn('unknown field', refused['detail'])
        self.assertLess(len(refused['detail']), 8192)
        self.assertEqual(client.request('inspect', bot='other')['error'], 'bot_not_found')

    def test_a_running_bot_forks_at_its_newest_finished_round(self):
        client = self.client()
        client.request('create', bot='Bob', workspace=str(self.path))
        turn = client.request('submit', bot='Bob', request_id='hold', prompt='wait')['result']['turn']
        self.model.requests.get(timeout=3)
        busy = client.request('submit', bot='Bob', request_id='more', prompt='hi')
        self.assertEqual(busy['error'], 'bot_busy')
        self.assertEqual(busy['running_turn'], turn)
        self.assertTrue(busy['detail'].startswith(f'turn {turn} is running; submit with delivery "steer"'),
                        busy['detail'])
        # No round has finished, so the fork starts at the turn's prompt.
        events = client.request('events', bot='Bob', after=0, limit=256)['result']['events']
        prompt = next(e['data']['node'] for e in events if e['event'] == 'accepted' and e['turn'] == turn)
        forked = client.request('fork', source='Bob', bot='Side', workspace=str(self.path))['result']
        self.assertEqual(forked['head'], prompt)
        self.assertEqual(busy['fork_point'], prompt)
        client.request('interrupt', bot='Bob', turn=turn)
        self.assertEqual(client.finished(turn)['data']['status'], 'interrupted')

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
        # The turn that named a folder moved Bob there.
        self.assertEqual(client.request('inspect', bot='Bob')['result']['workspace'], str(other.resolve()))
        # A bot created without a workspace needs one named until a turn gives it one.
        self.assertIsNone(client.request('create', bot='Nomad')['result']['workspace'])
        self.assertEqual(client.request('submit', bot='Nomad', request_id='n', prompt='hi')['error'], 'workspace_required')
        turn = client.request('submit', bot='Nomad', request_id='n2', prompt='hi', workspace=str(other))['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertEqual(client.request('inspect', bot='Nomad')['result']['workspace'], str(other.resolve()))

    def test_interrupt_and_crash_recovery_are_explicit(self):
        client = self.client()
        client.request('create', bot='Bob', workspace=str(self.path))
        turn = client.request('submit', bot='Bob', request_id='wait-1', prompt='wait')['result']['turn']
        self.model.requests.get(timeout=3)
        self.assertEqual(client.request('interrupt', bot='Bob', turn=turn+1)['error'], 'turn_not_found')
        client.request('interrupt', bot='Bob', turn=turn)
        self.assertEqual(client.finished(turn)['data']['status'], 'interrupted')
        pending = client.request('submit', bot='Bob', request_id='wait-2', prompt='wait')['result']['turn']
        self.model.requests.get(timeout=3)
        client.close(kill=True)
        client = self.client()
        self.assertEqual(client.request('inspect', bot='Bob')['result']['status'], 'interrupted')
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
        self.assertEqual(client.request('inspect', bot='Bob')['result']['status'], 'interrupted')
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
        result = json.loads(node_item(client, 'Bob', node)['result']['output'])
        self.assertEqual((result['stdout'], result['stderr'], result['exit_code']), ('stdout', 'stderr', 0))
        turn = client.request('submit', bot='Bob', request_id='cancel',
                              prompt='shell:sleep 30 & echo $! > child.pid; wait')['result']['turn']
        deadline = time.monotonic() + 3
        # The shell makes the file before it writes the PID.
        while not (self.path / 'child.pid').exists() or not (self.path / 'child.pid').read_text().endswith('\n'):
            self.assertLess(time.monotonic(), deadline)
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
        output = json.loads(node_item(client, 'Bob', killed[0]['data']['node'])['result']['output'])
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
        self.assertEqual(client.request('inspect', bot='Bob')['result']['status'], 'running')
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
                self.assertEqual(client.request('inspect', bot='Bob')['result']['status'], 'running')
            finally:
                alias.unlink()
        client.request('interrupt', bot='Bob', turn=turn)
        client.finished(turn)

    def test_large_event_replay_pages_preserve_all_events_and_service_liveness(self):
        client = self.client()
        client.request('create', bot='Bob', workspace=str(self.path))
        # Call ids at the 64 KiB bound, enough of them to need several pages.
        for index in range(12):
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
        self.assertEqual(sum(e['event'] == 'tool_started' for e in events), 12)
        self.assertEqual(sum(e['event'] == 'tool_completed' for e in events), 12)
        self.assertEqual(sum(e['event'] == 'turn_finished' for e in events), 12)
        self.assertEqual(client.request('inspect', bot='Bob')['result']['status'], 'completed')

    def test_distinct_store_files_do_not_collide_and_symlink_resume_is_exact(self):
        client = self.client()
        client.request('create', bot='Bob', workspace=str(self.path))
        other = Client(self.binary, self.path / 'state.db', self.url)
        self.addCleanup(other.close)
        self.assertEqual(other.request('inspect', bot='Bob')['error'], 'bot_not_found')
        client.close()
        alias = self.path / 'alias.sqlite'
        alias.symlink_to(self.path / 'state.sqlite')
        resumed = Client(self.binary, alias, self.url)
        self.addCleanup(resumed.close)
        self.assertEqual(resumed.request('inspect', bot='Bob')['result']['status'], 'idle')
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
        output = node_item(client, 'Bob', node)['result']['output']
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
                item = node_item(client, 'Bob', event['data']['node'])['result']
                self.assertFalse(sentinel in json.dumps(item))
        self.assertEqual(len(self.model.auth_checks), 4)
        self.assertTrue(all(self.model.auth_checks))

    def test_config_shell_excludes_leave_variables_out_of_tool_shells(self):
        store = self.path / 'state.sqlite'
        common = ['--store', str(store), '--provider', f'openai=responses,{self.url}',
                  '--model', 'openai/synthetic-model', '--tools', 'shell']
        env = {**clean_env(), 'HOME': str(self.path), 'SYNTHETIC_API_KEY': 'synthetic-a',
               'GITHUB_TOKEN': 'synthetic-b', 'AWS_PROFILE': 'synthetic-c', 'KEEP_ME': 'kept'}
        shown = 'shell:printf "%s:%s:%s:%s" "${SYNTHETIC_API_KEY-unset}" "${GITHUB_TOKEN-unset}" ' \
                '"${AWS_PROFILE-unset}" "$KEEP_ME" > seen'

        def seen(bot):
            run = subprocess.run([str(self.binary), 'run', *common, '--new', '--bot', bot, shown],
                                 env=env, capture_output=True, text=True, timeout=30, cwd=self.path)
            self.assertEqual(run.returncode, 0, run.stderr)
            return (self.path / 'seen').read_text()

        def shutdown():
            subprocess.run([str(self.binary), 'shutdown', '--store', str(store)],
                           env=env, capture_output=True, timeout=10)
        self.addCleanup(shutdown)
        # As in Codex, nothing past the providers' keys is left out by default.
        self.assertEqual(seen('Bob'), 'synthetic-a:synthetic-b:synthetic-c:kept')
        shutdown()
        (self.path / '.agent').mkdir()
        (self.path / '.agent/config.toml').write_text(
            '[shell_environment_policy]\nignore_default_excludes = false\nexclude = ["aws_*"]\n')
        # A daemon started after takes the defaults and the listed names.
        self.assertEqual(seen('Ann'), 'unset:unset:unset:kept')

    def test_history_pages_recover_an_omitted_long_message(self):
        client = self.client(tools='echo,history', settings={'context_items': 6})
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
        # Cut Bob's stored budget below the long message; settings are fixed
        # at creation, so the test edits the store.
        with closing(sqlite3.connect(self.path / 'state.sqlite')) as db, db:
            db.execute('''UPDATE bots SET settings='{"context_items":6,"context_bytes":65536}' WHERE name='Bob' ''')
        client = self.client(tools='echo,history')
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
        client = self.client(tools='echo,history', settings={'context_items': 6})
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
        final = poll(client, 'Bob', turn)['result']['text']
        self.assertTrue(final.startswith('echo:'))
        read = json.loads(final[5:])
        self.assertEqual(read['turn'], 1)
        self.assertIn('p1', read['text'])
        self.assertIn('reply:p1', read['text'])
        self.assertNotIn('p2', read['text'])
        turn = client.request('submit', bot='Bob', request_id='h9', prompt='history:9')['result']['turn']
        client.finished(turn)
        self.assertIn('turn_not_in_history', poll(client, 'Bob', turn)['result']['text'])
        self.assertIn('or with query to search them for text', texts[3][0])
        # A search finds text in the omitted turns, and not in the turn it
        # runs in, whose prompt names the same text.
        prompt = 'calls:' + json.dumps([['history', {'query': 'REPLY:P2'}]])
        turn = client.request('submit', bot='Bob', request_id='s', prompt=prompt)['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        found = json.loads(poll(client, 'Bob', turn)['result']['text'][5:])
        self.assertEqual([m['turn'] for m in found['matches']], [2])
        self.assertIn('reply:p2', found['matches'][0]['text'])
        self.assertTrue(found['done'])
        # A fork sees the same lineage and computes its own window over it.
        client.request('fork', source='Bob', bot='branch', workspace=str(self.path))
        turn = client.request('submit', bot='branch', request_id='b', prompt='history:2')['result']['turn']
        client.finished(turn)
        self.assertIn('reply:p2', poll(client, 'branch', turn)['result']['text'])
        while not self.model.requests.empty():
            requests.append(self.model.requests.get())
        self.assertTrue(requests[-2]['input'][0]['content'][0]['text'].startswith('[context note]'))

    def test_a_terminal_takes_input_across_turns_and_ends_on_its_handle(self):
        client = self.client('shell,write_stdin,wait')
        for bot in ('Bob', 'Eve'):
            client.request('create', bot=bot, workspace=str(self.path),
                           tools=['shell', 'write_stdin', 'wait'])

        def call(bot, request_id, name, arguments):
            prompt = 'calls:' + json.dumps([[name, arguments]])
            turn = client.request('submit', bot=bot, request_id=request_id, prompt=prompt)['result']['turn']
            self.assertEqual(client.finished(turn)['data']['status'], 'completed')
            return json.loads(poll(client, bot, turn)['result']['text'][5:])

        started = call('Bob', 't1', 'shell', {'command': "read -p 'name? ' n; echo \"hi $n\"",
                                              'tty': True, 'yield_time_ms': 1000})
        self.assertTrue(started['running'])
        self.assertEqual(started['output'], 'name? ')
        handle = started['handle']
        # Only the bot that started it types into it.
        prompt = 'calls:' + json.dumps([['write_stdin', {'handle': handle, 'chars': 'x\n'}]])
        turn = client.request('submit', bot='Eve', request_id='e1', prompt=prompt)['result']['turn']
        client.finished(turn)
        self.assertIn('terminal_not_running', poll(client, 'Eve', turn)['result']['text'])
        # A later turn types into it, and sees it end.
        typed = call('Bob', 't2', 'write_stdin', {'handle': handle, 'chars': 'bob\n', 'yield_time_ms': 5000})
        self.assertEqual(typed['output'], 'bob\nhi bob\n')
        self.assertEqual(typed['exit_code'], 0)
        ended = call('Bob', 't3', 'wait', {'handles': [handle]})
        self.assertEqual(ended['results'][handle]['exit_code'], 0)
        # One still running ends with the daemon, which stops at once.
        self.assertTrue(call('Bob', 't4', 'shell', {'command': 'sleep 600', 'tty': True,
                                                    'yield_time_ms': 0})['running'])
        started = time.monotonic()
        client.close()
        self.assertLess(time.monotonic() - started, 3)

    def test_a_terminal_poll_past_the_hold_parks_its_turn_off_the_active_slot(self):
        client = self.client('echo,shell,write_stdin,wait', extra=('--max-active', '1'),
                             settings={'approval_hold_ms': 100})
        for bot in ('Bob', 'Eve'):
            client.request('create', bot=bot, workspace=str(self.path),
                           tools=['echo', 'shell', 'write_stdin', 'wait'])

        def submit(bot, request_id, name, arguments):
            prompt = 'calls:' + json.dumps([[name, arguments]])
            return client.request('submit', bot=bot, request_id=request_id, prompt=prompt)['result']['turn']

        def result(bot, turn):
            self.assertEqual(client.finished(turn)['data']['status'], 'completed')
            return json.loads(poll(client, bot, turn)['result']['text'][5:])

        started = result('Bob', submit('Bob', 's', 'shell', {
            'command': 'printf ready; while [ ! -f release ]; do sleep .01; done; printf done',
            'tty': True, 'yield_time_ms': 1000}))
        self.assertEqual((started['output'], started['running']), ('ready', True))
        handle = started['handle']
        # A long poll parks its turn after the hold, waiting on the terminal.
        turn = submit('Bob', 'p', 'write_stdin', {'handle': handle, 'yield_time_ms': 30000})
        waiting = client.receive(lambda m: m.get('event') == 'turn_waiting' and m.get('turn') == turn)
        self.assertEqual(waiting['data']['handles'], [handle])
        # It holds no slot: another bot's turn runs to its end meanwhile.
        other = submit('Eve', 'e', 'echo', {'text': 'hi'})
        self.assertEqual(client.finished(other)['data']['status'], 'completed')
        # The terminal's end wakes it, with what it wrote and how it ended.
        began = time.monotonic()
        (self.path / 'release').touch()
        ended = result('Bob', turn)
        self.assertLess(time.monotonic() - began, 10)
        self.assertEqual((ended['output'], ended['exit_code'], ended['handle']), ('done', 0, handle))
        # A poll whose time runs out reports the terminal still running.
        handle = result('Bob', submit('Bob', 's2', 'shell', {
            'command': 'sleep 600', 'tty': True, 'yield_time_ms': 0}))['handle']
        turn = submit('Bob', 'p2', 'write_stdin', {'handle': handle, 'yield_time_ms': 400})
        client.receive(lambda m: m.get('event') == 'turn_waiting' and m.get('turn') == turn)
        lapsed = result('Bob', turn)
        self.assertEqual((lapsed['output'], lapsed['running'], lapsed['handle']), ('', True, handle))
        # A poll within the hold stays live.
        turn = submit('Bob', 'p3', 'write_stdin', {'handle': handle, 'yield_time_ms': 50})
        self.assertTrue(result('Bob', turn)['running'])
        events = client.request('events', bot='Bob', after=0, limit=256)['result']['events']
        self.assertFalse(any(e['event'] == 'turn_waiting' and e['turn'] == turn for e in events))

    def test_retention_prunes_records_and_deletes_idle_bots(self):
        client = self.client(settings={'keep_turns': 2})
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
        self.assertEqual(node_item(client, 'Bob', first)['result']['content'][0]['text'], 'reply:p2')
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
        self.assertEqual(client.request('inspect', bot='Bob')['error'], 'bot_not_found')
        self.assertEqual(client.request('delete', bot='Bob')['error'], 'bot_not_found')
        self.assertEqual(client.request('bots')['result']['bots'], [])

    def test_retention_preserves_background_completion_and_stale_turn_identity(self):
        client = self.client(tools='shell,wait', settings={'keep_turns': 1})
        client.request('create', bot='Bob', workspace=str(self.path))
        old = client.request('submit', bot='Bob', request_id='bg',
                             prompt='bg:while [ ! -f release ]; do sleep .01; done; printf done')['result']['turn']
        client.finished(old)
        text = poll(client, 'Bob', old)['result']['text']
        handle = json.loads(text.removeprefix('echo:'))['handle']
        self.assertEqual(client.request('delete', bot='Bob')['error'], 'bot_busy')
        later = client.request('submit', bot='Bob', request_id='next', prompt='next')['result']['turn']
        client.finished(later)
        self.assertEqual(poll(client, 'Bob', old)['error'], 'turn_result_pruned')
        waited = client.request('wait', handles=[f'turn:Bob/{old}'], timeout_ms=100)['result']['results']
        # The turn's view stays; only how it ended is gone.
        pruned = waited[f'turn:Bob/{old}']
        self.assertEqual((pruned['error'], pruned['status'], pruned['bot']), ('turn_result_pruned', 'completed', 'Bob'))
        (self.path / 'release').touch()
        waited = client.request('wait', handles=[handle], timeout_ms=3000)['result']['results']
        self.assertEqual(waited[handle]['stdout'], 'done')
        self.assertIn('result', client.request('delete', bot='Bob'))
        client.request('shutdown')
        client.close()
        client = self.client(tools='shell,wait', settings={'keep_turns': 1})
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
        big = [client.request('create', bot=bot, workspace=str(self.path))['result'] for bot in ('Big', 'Other')][0]
        for n in range(40):
            turn = client.request('submit', bot='Big', request_id=str(n), prompt='shell:printf big')['result']['turn']
            self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        before = client.request('stats')['result']['store']['operations'].get('delete_bot', {}).get('count', 0)
        # A delete resent while the first still runs succeeds beside it; the
        # pieces each one took add up to the bot.
        sent = []
        for _ in range(2):
            client.next_id += 1
            sent.append(client.next_id)
            client.process.stdin.write(json.dumps({'id': client.next_id, 'op': 'delete', 'bot': 'Big',
                                                   'bot_id': big['bot_id']}) + '\n')
        client.process.stdin.flush()
        freed = [client.receive(lambda m, i=i: m.get('id') == i)['result'] for i in sent]
        self.assertEqual(sorted(f['duplicate'] for f in freed), [False, True])
        self.assertEqual(sum(f['turns'] for f in freed), 40)
        after = client.request('stats')['result']['store']['operations']['delete_bot']['count']
        # 40 turns of records in pieces of 16, then the turn rows, then nodes, then the bot.
        self.assertGreaterEqual(after - before, 5)
        self.assertEqual(client.request('inspect', bot='Big')['error'], 'bot_not_found')
        turn = client.request('submit', bot='Other', request_id='o', prompt='hi')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')

    def test_bot_identities_outlive_names_and_refuse_stale_retries(self):
        client = self.client()
        bob = client.request('create', bot='Bob', workspace=str(self.path))['result']
        first = client.request('submit', bot='Bob', request_id='r7', bot_id=bob['bot_id'], prompt='hello')['result']
        self.assertEqual((first['bot_id'], first['duplicate']), (bob['bot_id'], False))
        checkpoint = client.finished(first['turn'])['data']['checkpoint']
        fork = client.request('fork', source='Bob', checkpoint=checkpoint, bot='Fork', workspace=str(self.path))['result']
        self.assertNotEqual(fork['bot_id'], bob['bot_id'])
        # The request namespace is per identity: the fork never ran r7.
        forked = client.request('submit', bot='Fork', request_id='r7', bot_id=fork['bot_id'], prompt='hello')['result']
        self.assertEqual((forked['duplicate'], forked['bot_id']), (False, fork['bot_id']))
        client.finished(forked['turn'])
        self.assertEqual(client.request('submit', bot='Fork', request_id='r7', bot_id=bob['bot_id'], prompt='hello')['error'],
                         'bot_not_found')
        # Identities survive restart and never move to another name.
        client.close(kill=True)
        client = self.client()
        listed = {b['name']: b['bot_id'] for b in client.request('bots')['result']['bots']}
        self.assertEqual(listed, {'Bob': bob['bot_id'], 'Fork': fork['bot_id']})
        self.assertEqual(client.request('inspect', bot='Bob')['result']['bot_id'], bob['bot_id'])
        again = client.request('submit', bot='Bob', request_id='r7', bot_id=bob['bot_id'], prompt='hello')['result']
        self.assertEqual((again['duplicate'], again['turn']), (True, first['turn']))
        # A recycled name is a new identity: a stale retry is refused, a plain one is fresh work.
        client.request('delete', bot='Bob')
        reborn = client.request('create', bot='Bob', workspace=str(self.path))['result']
        self.assertGreater(reborn['bot_id'], fork['bot_id'])
        stale = client.request('submit', bot='Bob', request_id='r7', bot_id=bob['bot_id'], prompt='hello')
        self.assertEqual(stale['error'], 'bot_not_found')
        self.assertIn(str(reborn['bot_id']), stale['detail'])
        fresh = client.request('submit', bot='Bob', request_id='r7', prompt='hello')['result']
        self.assertEqual((fresh['duplicate'], fresh['bot_id']), (False, reborn['bot_id']))
        self.assertNotEqual(fresh['turn'], first['turn'])
        client.finished(fresh['turn'])
        self.assertEqual(client.request('submit', bot='Gone', request_id='r7', bot_id=bob['bot_id'], prompt='hello')['error'],
                         'bot_not_found')

    def test_a_server_error_retries_after_the_wait_its_response_names(self):
        client = self.client()
        client.request('create', bot='Bob', workspace=str(self.path))
        turn = client.request('submit', bot='Bob', request_id='r1', prompt='busy:1')['result']['turn']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        retried = [m['data'] for m in client.saved if m.get('event') == 'retry']
        # Its Retry-After, not the 250 ms backoff, as the providers' SDKs take it.
        self.assertEqual([(r['error'], r['delay_ms']) for r in retried], [('provider_http_503', 700)])

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
                self.assertEqual([(r['data']['attempt'], r['data']['error']) for r in retries], [(1, code)])
        turns = client.request('turns', bot='Bob')['result']['turns']
        self.assertEqual([t['retries'] for t in turns], [1, 1, 1, 1, 1])
        # Each retry stays in the bot's events, for whoever reads them later.
        kept = client.request('events', bot='Bob', after=0, limit=256)['result']['events']
        self.assertEqual([e['data']['error'] for e in kept if e['event'] == 'retry'],
                         ['provider_http_503', 'provider_http_429', 'provider_rate_limited',
                          'provider_rate_limited', 'provider_http_520'])
        # The retry waited for the pool's Retry-After, then for the delay
        # named in the stream, which a top-level error names the same way.
        # Each is timed here from the refused attempt's arrival, before the
        # refusal, so a slow machine only lengthens it.
        arrived = self.model.arrived
        for prompt, wait in (('limited:1', .05), ('streamlimit:1', .3), ('streamlimit-flat:1', .3)):
            self.assertGreaterEqual(arrived[prompt, 2] - arrived[prompt, 1], wait, prompt)
        # Headers saying the allowance is spent pace the next call; nothing fails.
        self.assertEqual(run('paced:1')[0]['status'], 'completed')
        data, retries = run('hello')
        self.assertEqual((data['status'], retries), ('completed', []))
        held = client.request('turns', bot='Bob')['result']['turns'][-1]
        self.assertEqual(held['retries'], 0)
        self.assertGreater(held['paced_ms'], 0)
        self.assertGreaterEqual(arrived['hello', 1] - arrived['paced:1', 1], .3)
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
