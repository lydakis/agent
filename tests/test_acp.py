"""`sonny acp`, as T3 Code runs it: a thread is its folder's lead, its turns
and calls are the thread's updates, a call waiting for the person is asked
in the thread, and a task the lead starts opens as a thread of its own,
through T3 Code's tools, which that thread then follows."""
import http.server
import json
import os
import queue
import subprocess
import threading
import time
import unittest

from bench.socket_client import SocketClient
from bench.targets import clean_env
from tests.test_runtime import ModelFixture
from tests.test_served import declared


class T3Tools(http.server.BaseHTTPRequestHandler):
    """T3 Code's MCP endpoint, as small as `t3_thread_launch` needs."""

    def log_message(self, *_):
        pass

    def reply(self, status, body=None, session=None):
        data = json.dumps(body).encode() if body is not None else b''
        self.send_response(status)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(data)))
        if session:
            self.send_header('mcp-session-id', session)
        self.end_headers()
        self.wfile.write(data)

    def do_POST(self):
        message = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        self.server.headers.append(dict(self.headers))
        if message['method'] == 'initialize':
            return self.reply(200, {'jsonrpc': '2.0', 'id': message['id'],
                                    'result': {'protocolVersion': '2025-06-18', 'capabilities': {'tools': {}},
                                               'serverInfo': {'name': 't3'}}}, session='s1')
        if 'id' not in message:
            return self.reply(202)
        self.server.calls.put(message['params'])
        result = {'threadId': 'thread-2', 'link': 't3://thread/thread-2', 'status': 'started'}
        self.reply(200, {'jsonrpc': '2.0', 'id': message['id'],
                         'result': {'content': [{'type': 'text', 'text': json.dumps(result)}],
                                    'structuredContent': result}})

    def do_DELETE(self):
        self.reply(200)


class Thread:
    """One `sonny acp` process, as T3 Code drives it over stdio."""

    def __init__(self, test, env, cwd):
        self.process = subprocess.Popen(
            [str(test.sonny), 'acp', '--store', str(test.store), '--socket', str(test.daemon.socket_path)],
            env=env, cwd=cwd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
        test.addCleanup(self.close)
        self.messages = queue.Queue()
        self.next = 0
        threading.Thread(target=self.read, daemon=True).start()

    def read(self):
        for line in self.process.stdout:
            self.messages.put(json.loads(line))

    def close(self):
        if self.process.poll() is None:
            self.process.kill()
        self.process.wait(timeout=5)
        self.process.stdin.close()
        self.process.stdout.close()

    def send(self, message):
        self.process.stdin.write(json.dumps({'jsonrpc': '2.0', **message}) + '\n')
        self.process.stdin.flush()

    def ask(self, method, params):
        self.next += 1
        self.send({'id': self.next, 'method': method, 'params': params})
        return self.next

    def until(self, done, timeout=30):
        """Every message up to and including the first `done` takes."""
        seen = []
        deadline = time.monotonic() + timeout
        while True:
            left = deadline - time.monotonic()
            if left <= 0:
                raise AssertionError(f'timed out after {seen}')
            try:
                message = self.messages.get(timeout=left)
            except queue.Empty:
                continue
            seen.append(message)
            if done(message):
                return seen

    def answer_of(self, request):
        seen = self.until(lambda m: m.get('id') == request and 'method' not in m)
        return seen[-1], [m['params']['update'] for m in seen if m.get('method') == 'session/update']


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class AcpTests(ModelFixture):
    def setUp(self):
        super().setUp()
        self.sonny = self.binary.parent / 'sonny'
        self.store = self.path / 'state.sqlite'
        self.daemon = SocketClient(self.binary, self.store, self.url, 'echo')
        self.addCleanup(self.daemon.close)
        self.t3 = http.server.ThreadingHTTPServer(('127.0.0.1', 0), T3Tools)
        self.t3.calls, self.t3.headers = queue.Queue(), []
        threading.Thread(target=self.t3.serve_forever, daemon=True).start()
        self.addCleanup(self.t3.server_close)
        self.addCleanup(self.t3.shutdown)
        self.env = {**clean_env(), 'HOME': str(self.path), 'AGENT_BIN': str(self.binary),
                    'GIT_AUTHOR_NAME': 't', 'GIT_AUTHOR_EMAIL': 't@example.com',
                    'GIT_COMMITTER_NAME': 't', 'GIT_COMMITTER_EMAIL': 't@example.com',
                    'T3_ACP_MCP_ENDPOINT': f'http://127.0.0.1:{self.t3.server_port}/mcp',
                    'T3_ACP_MCP_AUTHORIZATION': 'Bearer thread-1'}
        # The lookout the threads find running, judging nothing.
        lookout = subprocess.Popen([str(self.sonny), 'lookout', '--judge', 'off', '--store', str(self.store),
                                    '--socket', str(self.daemon.socket_path)],
                                   env=self.env, stdout=subprocess.PIPE, text=True)
        self.addCleanup(lambda: (lookout.kill(), lookout.wait(timeout=5), lookout.stdout.close()))
        self.assertEqual(json.loads(lookout.stdout.readline())['event'], 'watching')

    def repo(self):
        repo = self.path / 'calc'
        repo.mkdir()
        for args in (['init', '-q', '-b', 'main'], ['commit', '-q', '--allow-empty', '-m', 'one']):
            subprocess.run(['git', *args], cwd=repo, env=self.env, check=True, capture_output=True)
        return repo

    def open(self, cwd):
        thread = Thread(self, self.env, cwd)
        initialized, _ = thread.answer_of(thread.ask('initialize', {
            'protocolVersion': 2, 'clientCapabilities': {}, 'clientInfo': {'name': 't3-code'}}))
        self.assertEqual(initialized['result']['protocolVersion'], 2)
        self.assertEqual(initialized['result']['agentInfo']['name'], 'sonny')
        opened, _ = thread.answer_of(thread.ask('session/new', {'cwd': str(cwd), 'mcpServers': []}))
        return thread, opened['result']['sessionId']

    @staticmethod
    def wrapped(text):
        """A prompt as T3 Code sends it: in its own instructions, then its
        note as a block of its own."""
        return [{'type': 'text', 'text': '<t3_code_instructions>\nUse delegate_task.\n</t3_code_instructions>\n\n'
                                         f'<user_request>\n{text}\n</user_request>'},
                {'type': 'text', 'text': "<runtime_info>In case you're asked: you are running in T3 Code.</runtime_info>"
                                         '\n\n<pull_request_linking>Link pull requests.</pull_request_linking>'}]

    def test_a_thread_is_its_folders_lead_and_a_task_it_starts_opens_as_a_thread_of_its_own(self):
        repo = self.repo()
        self.daemon.request('create', bot='calc.lead', workspace=str(repo), model='openai/synthetic-model',
                            instructions='Lead.', declare=[declared('start_task', inherit=False)],
                            approve=['start_task'], approver='manual')
        thread, session = self.open(repo)
        self.model.call_script = [('start_task', {'name': 'fix', 'brief': 'Fix calc.py.'})]
        prompt = thread.ask('session/prompt', {'sessionId': session, 'prompt': self.wrapped('script')})
        # The call waits for the person, who is asked in the thread.
        [asked] = [m for m in thread.until(lambda m: m.get('method') == 'session/request_permission')
                   if m.get('method') == 'session/request_permission']
        self.assertEqual(asked['params']['toolCall']['name'], 'start_task')
        self.assertEqual([o['kind'] for o in asked['params']['options']], ['allow_once', 'reject_once'])
        thread.send({'id': asked['id'], 'result': {'outcome': {'outcome': 'selected', 'optionId': 'allow'}}})
        answered, updates = thread.answer_of(prompt)
        self.assertEqual(answered['result'], {'stopReason': 'end_turn'})
        kinds = [u['sessionUpdate'] for u in updates]
        self.assertEqual(kinds[-1], 'state_update')
        self.assertEqual(updates[-1]['state'], 'idle')
        [started] = [u for u in updates if u['sessionUpdate'] == 'tool_call']
        self.assertEqual((started['title'], started['kind']), ('start_task', 'other'))
        [ended] = [u for u in updates if u['sessionUpdate'] == 'tool_call_update' and 'status' in u]
        self.assertEqual(ended['status'], 'completed')
        self.assertIn('Started calc.fix', ended['content'][0]['content']['text'])
        self.assertIn('done', ''.join(u['content']['text'] for u in updates
                                      if u['sessionUpdate'] == 'agent_message_chunk'))
        # The thread was the folder's lead, which T3 Code's words never
        # reached: Sonny took the person's out of them.
        [turn] = self.daemon.request('turns', bot='calc.lead')['result']['turns']
        prompts = self.daemon.request('prompts', bot='calc.lead', turn=turn['turn'])['result']['prompts']
        self.assertEqual(prompts[0]['text'], 'script')

        # The task opens as a thread of its own, in its worktree, with its
        # brief as the first message, through this thread's own tools.
        launch = self.t3.calls.get(timeout=30)
        self.assertEqual(launch['name'], 't3_thread_launch')
        worktree = self.path / '.agent/worktrees/calc.fix'
        turns = self.daemon.request('turns', bot='calc.fix')['result']['turns']
        brief = self.daemon.request('prompts', bot='calc.fix', turn=turns[0]['turn'])['result']['prompts'][0]['text']
        self.assertEqual(launch['arguments']['title'], 'fix')
        self.assertEqual(launch['arguments']['message'], brief)
        self.assertEqual(launch['arguments']['workspaceStrategy'],
                         {'type': 'existing_worktree', 'worktreePath': str(worktree)})
        self.assertTrue(all(h.get('authorization') == 'Bearer thread-1'
                            or h.get('Authorization') == 'Bearer thread-1' for h in self.t3.headers))

        # That thread is the task's, not a lead made for its folder: it
        # answers with the task's work.
        child, child_session = self.open(worktree)
        answered, updates = child.answer_of(child.ask('session/prompt', {
            'sessionId': child_session, 'prompt': self.wrapped(brief)}))
        self.assertEqual(answered['result']['stopReason'], 'end_turn')
        self.assertTrue([u for u in updates if u['sessionUpdate'] == 'agent_message_chunk'])
        kept = self.path / '.agent/sonny/t3/sessions' / child_session
        self.assertEqual(kept.read_text(), 'calc.fix')
        self.assertEqual([b['name'] for b in self.daemon.request('bots')['result']['bots']],
                         ['calc.fix', 'calc.lead'])

    def test_a_prompt_that_steers_is_answered_when_the_turn_it_joined_ends(self):
        repo = self.repo()
        self.daemon.request('create', bot='calc.lead', workspace=str(repo), model='openai/synthetic-model',
                            instructions='Lead.', declare=[declared('start_task', inherit=False)],
                            approve=['start_task'], approver='manual')
        thread, session = self.open(repo)
        self.model.call_script = [('start_task', {'name': 'fix', 'brief': 'Fix calc.py.'})]
        first = thread.ask('session/prompt', {'sessionId': session, 'prompt': self.wrapped('script')})
        [asked] = [m for m in thread.until(lambda m: m.get('method') == 'session/request_permission')
                   if m.get('method') == 'session/request_permission']
        # A second prompt while the turn runs steers it: the first prompt's
        # words are in, and the second waits on the turn that takes it in.
        second = thread.ask('session/prompt', {'sessionId': session, 'prompt': self.wrapped('and the tests')})
        answered, _ = thread.answer_of(first)
        self.assertEqual(answered['result'], {'stopReason': 'end_turn'})
        thread.send({'id': asked['id'], 'result': {'outcome': {'outcome': 'selected', 'optionId': 'allow'}}})
        answered, updates = thread.answer_of(second)
        self.assertEqual(answered['result'], {'stopReason': 'end_turn'})
        self.assertEqual(updates[-1]['state'], 'idle')
        turns = self.daemon.request('turns', bot='calc.lead')['result']['turns']
        self.assertEqual(sorted(t['status'] for t in turns), ['completed', 'steered'])
        self.assertEqual(self.t3.calls.get(timeout=30)['name'], 't3_thread_launch')

    def test_a_turn_the_lead_takes_on_its_own_shows_in_the_thread(self):
        repo = self.repo()
        self.daemon.request('create', bot='calc.lead', workspace=str(repo), model='openai/synthetic-model',
                            instructions='Lead.')
        thread, session = self.open(repo)
        # A model that reasons before it answers: the reply, which
        # streamed, is said once.
        self.model.history_reasoning = {'type': 'reasoning', 'id': 'rs_1',
                                        'summary': [{'type': 'summary_text', 'text': 'Thought.'}]}
        answered, updates = thread.answer_of(thread.ask('session/prompt', {'sessionId': session,
                                                                            'prompt': self.wrapped('hello')}))
        self.assertEqual(answered['result'], {'stopReason': 'end_turn'})
        self.assertEqual(''.join(u['content']['text'] for u in updates
                                 if u['sessionUpdate'] == 'agent_message_chunk'), 'reply:hello')
        self.model.history_reasoning = None
        # The lookout wakes the lead, as when a task finished: no prompt of
        # the thread's, so the thread is told what started it.
        self.daemon.request('submit', bot='calc.lead', request_id='lookout-1', prompt='fix finished')
        seen = thread.until(lambda m: m.get('params', {}).get('update', {}).get('state') == 'idle')
        updates = [m['params']['update'] for m in seen]
        self.assertEqual(updates[0], {'sessionUpdate': 'state_update', 'state': 'running'})
        [said] = [u for u in updates if u['sessionUpdate'] == 'user_message']
        self.assertEqual(said['content'], [{'type': 'text', 'text': 'fix finished'}])
        self.assertTrue([u for u in updates if u['sessionUpdate'] == 'agent_message_chunk'])

    def test_a_thread_sonny_does_not_know_does_not_come_back(self):
        repo = self.repo()
        thread = Thread(self, self.env, repo)
        thread.answer_of(thread.ask('initialize', {'protocolVersion': 2, 'clientCapabilities': {}}))
        refused, _ = thread.answer_of(thread.ask('session/resume', {'sessionId': 'sonny-gone', 'cwd': str(repo),
                                                                     'mcpServers': []}))
        self.assertIn('sonny-gone', refused['error']['message'])
        self.assertEqual(self.daemon.request('bots')['result']['bots'], [])
