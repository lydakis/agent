"""Actual Rust process, disk recovery, provider transport, and tool loop."""
import http.server
import json
import os
from pathlib import Path
import queue
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
            if hasattr(self.server, 'expected_authorization'):
                self.server.auth_checks.append(self.headers.get('Authorization') == self.server.expected_authorization)
            assert self.path == '/v1/responses'
            assert request['model'] == 'synthetic-model'
            user = [i for i in request['input'] if i.get('role') == 'user'][-1]['content'][0]['text']
            if user == 'gate':
                self.server.release_headers.wait(timeout=5)
            if user == 'wait':
                time.sleep(5)
            if user == 'burst':
                time.sleep(.3)  # Allow the CLI to attach its live follower.
            last = request['input'][-1]
            if last.get('type') == 'function_call_output':
                text = 'echo:' + last['output']
                output = [{'id': 'msg_echo', 'type': 'message', 'role': 'assistant',
                           'content': [{'type': 'output_text', 'text': text}]}]
            elif user.startswith('shell:'):
                text = ''
                output = [{'type': 'function_call', 'name': 'shell', 'call_id': 'shell-1',
                           'arguments': json.dumps({'command': user[6:], 'timeout_ms': 2000})}]
            elif user.startswith('tool:'):
                text = ''
                output = [{'type': 'function_call', 'name': 'echo', 'call_id': 'echo-1',
                           'arguments': json.dumps({'text': user[5:]})}]
            elif user == 'large-call-id':
                text = ''
                output = [{'type': 'function_call', 'name': 'echo', 'call_id': 'c' * 210000,
                           'arguments': json.dumps({'text': 'ok'})}]
            else:
                text = 'reply:' + user
                output = [{'id': 'msg_text', 'type': 'message', 'role': 'assistant',
                           'content': [{'type': 'output_text', 'text': text}]}]
            events = [{'type': 'response.created', 'response': {'id': 'response_test'}}]
            if text:
                events.append({'type': 'response.output_text.delta', 'delta': text})
            if user != 'truncate':
                events.append({'type': 'response.completed', 'response': {'status': 'completed', 'output': output}})
            self.send_response(200)
            self.send_header('Content-Type', 'text/event-stream')
            self.send_header('Transfer-Encoding', 'chunked')
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
                frame = ('data: ' + json.dumps(event, ensure_ascii=False) + '\r\n\r\n').encode()
                # Split inside UTF-8 sequences and SSE line boundaries.
                chunk_size = 8192 if user == 'large-call-id' else 7
                for offset in range(0, len(frame), chunk_size):
                    part = frame[offset:offset + chunk_size]
                    self.wfile.write(f'{len(part):x}\r\n'.encode() + part + b'\r\n')
            self.wfile.write(b'0\r\n\r\n')
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass


class AnthropicModel(http.server.BaseHTTPRequestHandler):
    """Synthetic Anthropic Messages endpoint: thinking, text, tool_use, tool_result."""
    protocol_version = 'HTTP/1.1'

    def log_message(self, *_):
        pass

    def do_POST(self):
        try:
            request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
            self.server.requests.put(request)
            assert self.path == '/v1/messages'
            assert self.headers.get('x-api-key') == 'synthetic-anthropic-key'
            assert self.headers.get('anthropic-version') == '2023-06-01'
            assert request['model'] == 'synthetic-claude' and request['stream'] and request['max_tokens'] > 0
            assert isinstance(request['system'], str)
            assert [t['name'] for t in request['tools']] == ['echo', 'shell']
            assert 'input_schema' in request['tools'][0]
            last = request['messages'][-1]
            assert last['role'] == 'user'
            blocks = [{'type': 'thinking', 'thinking': 'plan', 'signature': 'sig-1'}]
            if last['content'][0]['type'] == 'tool_result':
                blocks.append({'type': 'text', 'text': 'echo:' + last['content'][0]['content']})
                stop = 'end_turn'
            else:
                user = last['content'][0]['text']
                if user.startswith('tool:'):
                    blocks.append({'type': 'tool_use', 'id': 'toolu_1', 'name': 'echo', 'input': {'text': user[5:]}})
                    stop = 'tool_use'
                else:
                    blocks.append({'type': 'text', 'text': 'reply:' + user})
                    stop = 'end_turn'
            events = [('message_start', {'message': {'usage': {'input_tokens': 5, 'cache_read_input_tokens': 2}}})]
            for index, block in enumerate(blocks):
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
            events.append(('message_delta', {'delta': {'stop_reason': stop}, 'usage': {'output_tokens': 7}}))
            events.append(('message_stop', {}))
            self.send_response(200)
            self.send_header('Content-Type', 'text/event-stream')
            self.send_header('Transfer-Encoding', 'chunked')
            self.end_headers()
            for kind, body in events:
                frame = f'event: {kind}\ndata: {json.dumps({"type": kind, **body})}\n\n'.encode()
                self.wfile.write(f'{len(frame):x}\r\n'.encode() + frame + b'\r\n')
            self.wfile.write(b'0\r\n\r\n')
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class AnthropicRuntimeTests(unittest.TestCase):
    def test_messages_family_round_trips_thinking_tools_and_usage(self):
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
                        key_env='ANTHROPIC_TEST_KEY', env=env, provider='anthropic', family='anthropic')
        self.addCleanup(client.close)
        client.request('create', bot='Bob', workspace=str(path), reasoning='low')
        turn = client.request('submit', bot='Bob', request_id='r1', prompt='tool:shared')['result']['turn']
        finished = client.finished(turn)
        self.assertEqual(finished['data']['status'], 'completed')
        deltas = [m for m in client.saved if m.get('event') in ('text_delta', 'thinking_delta')]
        self.assertEqual([d['event'] for d in deltas][:2], ['thinking_delta', 'thinking_delta'])
        self.assertEqual(''.join(d['text'] for d in deltas if d['event'] == 'text_delta'), 'echo:shared')
        usage = [m for m in client.saved if m.get('event') == 'usage']
        self.assertEqual(len(usage), 2)
        self.assertEqual(usage[0]['data'], {'input_tokens': 5, 'output_tokens': 7, 'cached_input_tokens': 2})
        first, second = model.requests.get(timeout=1), model.requests.get(timeout=1)
        self.assertEqual(first['thinking'], {'type': 'enabled', 'budget_tokens': 2048})
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
        self.assertEqual(client.request('create', bot='Bad', workspace=str(path), reasoning='max')['error'],
                         'invalid_reasoning_level')
        self.assertEqual(client.request('create', bot='Bad', workspace=str(path), model='openai/x')['error'],
                         'provider_unavailable')


class ModelFixture(unittest.TestCase):
    def setUp(self):
        root = Path(__file__).resolve().parent.parent
        self.temp = tempfile.TemporaryDirectory(dir=root / '.local')
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name)
        class Server(http.server.ThreadingHTTPServer):
            request_queue_size = 128
        self.model = Server(('127.0.0.1', 0), Model)
        self.model.requests = queue.Queue()
        self.model.daemon_threads = True
        self.worker = threading.Thread(target=self.model.serve_forever, daemon=True)
        self.worker.start()
        self.addCleanup(self.model.server_close)
        self.addCleanup(self.model.shutdown)
        self.binary = root / '.local/target/release/agent'
        self.url = f'http://127.0.0.1:{self.model.server_port}/v1'

    def client(self, tools="echo"):
        client = Client(self.binary, self.path / 'state.sqlite', self.url, tools)
        self.addCleanup(client.close)
        return client



@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class RuntimeTests(ModelFixture):
    def test_stdio_shutdown_exits_and_releases_store_without_stdin_eof(self):
        client = self.client()
        self.addCleanup(lambda: client.close(kill=True))
        self.assertTrue(client.request('shutdown')['result']['shutting_down'])
        self.assertEqual(client.process.wait(timeout=2), 0)
        restarted = self.client()
        self.assertIn('result', restarted.request('bots'))

    def test_request_startup_is_bounded_but_established_streams_are_not(self):
        self.model.release_headers = threading.Event()
        self.model.all_streaming = threading.Barrier(70)
        self.addCleanup(self.model.release_headers.set)
        client = self.client()
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
        self.assertEqual(client.finished(turn)['data']['status'], 'uncertain')
        deadline = time.monotonic() + 2
        def alive():
            try:
                return child.is_running() and child.status() != psutil.STATUS_ZOMBIE
            except psutil.NoSuchProcess:
                return False
        while alive() and time.monotonic() < deadline:
            time.sleep(.01)
        self.assertFalse(alive())
        self.assertEqual(client.request('submit', bot='Bob', request_id='retry', prompt='hi')['error'], 'tool_outcome_uncertain')

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

    def test_premature_provider_eof_cannot_be_a_successful_checkpoint(self):
        client = self.client()
        client.request('create', bot='Bob', workspace=str(self.path))
        turn = client.request('submit', bot='Bob', request_id='truncated', prompt='truncate')['result']['turn']
        event = client.finished(turn)
        self.assertEqual(event['data']['status'], 'failed')
        self.assertIsNone(event['data']['checkpoint'])
        self.assertEqual(event['data']['error'], 'missing_completion')
