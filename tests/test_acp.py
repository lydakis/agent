"""`sonny acp`, as T3 Code runs it: a thread is a lead of its folder, its turns
and calls are the thread's updates, a call waiting for the person is asked
in the thread, and a task the lead starts opens as a thread of its own,
through T3 Code's tools, which that thread then follows."""
import http.server
import json
import re
import os
import queue
import subprocess
import threading
import time
import unittest

from bench.socket_client import SocketClient
from bench.targets import clean_env
from tests.test_runtime import SENT, ModelFixture
from tests.test_served import declared


class T3Tools(http.server.BaseHTTPRequestHandler):
    """T3 Code's MCP endpoint, as small as `delegate_task` and
    `t3_thread_launch` need. `delegate_task` fails, as when the lead's
    thread has no run on, while `server.delegating` is false."""

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
        if message['params']['name'] == 'delegate_task' and not self.server.delegating:
            refused = {'content': [{'type': 'text', 'text': 'parent_not_active'}], 'isError': True}
            return self.reply(200, {'jsonrpc': '2.0', 'id': message['id'], 'result': refused})
        result = {'threadId': 'thread-2', 'link': 't3://thread/thread-2', 'status': 'started'}
        if message['params']['name'] == 'delegate_task':
            result = {'taskId': 'node-1', 'childThreadId': 'thread-2', 'status': 'running'}
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

    @staticmethod
    def resumed(test, env, cwd, session):
        """A new process reopening `session`: its answer."""
        thread = Thread(test, env, cwd)
        thread.answer_of(thread.ask('initialize', {'protocolVersion': 2, 'clientCapabilities': {}}))
        answered = thread.answer_of(thread.ask('session/resume', {'sessionId': session, 'cwd': str(cwd),
                                                                   'mcpServers': []}))
        thread.close()
        return answered

    def answer_of(self, request):
        seen = self.until(lambda m: m.get('id') == request and 'method' not in m)
        return seen[-1], [m['params']['update'] for m in seen if m.get('method') == 'session/update']


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class AcpTests(ModelFixture):
    def setUp(self):
        super().setUp()
        self.sonny = self.binary.parent / 'sonny'
        self.store = self.path / 'state.sqlite'
        # Its companions, Sonny's lookout among them, have its environment.
        self.daemon = SocketClient(self.binary, self.store, self.url, 'echo',
                                   env={**clean_env(), 'HOME': str(self.path)})
        self.addCleanup(self.daemon.close)
        self.t3 = http.server.ThreadingHTTPServer(('127.0.0.1', 0), T3Tools)
        self.t3.calls, self.t3.headers, self.t3.delegating = queue.Queue(), [], True
        threading.Thread(target=self.t3.serve_forever, daemon=True).start()
        self.addCleanup(self.t3.server_close)
        self.addCleanup(self.t3.shutdown)
        self.env = {**clean_env(), 'HOME': str(self.path), 'AGENT_BIN': str(self.binary),
                    'GIT_AUTHOR_NAME': 't', 'GIT_AUTHOR_EMAIL': 't@example.com',
                    'GIT_COMMITTER_NAME': 't', 'GIT_COMMITTER_EMAIL': 't@example.com',
                    'T3_ACP_MCP_ENDPOINT': f'http://127.0.0.1:{self.t3.server_port}/mcp',
                    'T3_ACP_MCP_AUTHORIZATION': 'Bearer thread-1'}

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
        self.opened = opened
        return thread, opened['result']['sessionId']

    def adopt(self, thread, session, name='lead'):
        """The thread's first words, `/switch NAME`: it is that lead of the
        folder, which starts no turn."""
        answered, updates = thread.answer_of(thread.ask('session/prompt', {
            'sessionId': session, 'prompt': self.wrapped(f'/switch {name}')}))
        self.assertEqual(answered['result'], {'stopReason': 'end_turn'})
        bot = 'calc.lead' if name == 'lead' else f'calc.lead.{name}'
        self.assertEqual([u['content']['text'] for u in updates if u['sessionUpdate'] == 'agent_message_chunk'],
                         [f'This thread is {bot}.'])

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
        self.adopt(thread, session)
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
        # The tool host that answered it is the daemon's, not the thread's,
        # as is the lookout.
        companions = self.daemon.request('companions')['result']['companions']
        # Each runs by a name of its own, which is Sonny's program.
        names = self.path / '.agent/sonny/programs/names'
        self.assertEqual(sorted((c['name'], c['command'], c['args'][0]) for c in companions),
                         [('sonny-lookout', str(names / 'sonny-lookout'), 'lookout'),
                          ('sonny-tool-host', str(names / 'sonny-tool-host'), 'tool-host')])
        for companion in companions:
            self.assertTrue(os.path.samefile(companion['command'], self.sonny.resolve()))
            self.assertEqual(companion['env']['SONNY_PROGRAM'], str(self.sonny.resolve()))
        # The task was T3 Code's child of this prompt before it was answered.
        delegated = self.t3.calls.get_nowait()
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
        # The thread was the lead it asked for, which T3 Code's words never
        # reached: Sonny took the person's out of them.
        [turn] = self.daemon.request('turns', bot='calc.lead')['result']['turns']
        prompts = self.daemon.request('prompts', bot='calc.lead', turn=turn['turn'])['result']['prompts']
        # Stamped with when it was sent, as every message Sonny passes on.
        self.assertRegex(prompts[0]['text'], SENT)
        self.assertEqual(SENT.sub('', prompts[0]['text']), 'script')

        # The task opens as the lead thread's child, with its brief as the
        # first message, through this thread's own tools.
        self.assertEqual(delegated['name'], 'delegate_task')
        turns = self.daemon.request('turns', bot='calc.fix')['result']['turns']
        brief = self.daemon.request('prompts', bot='calc.fix', turn=turns[0]['turn'])['result']['prompts'][0]['text']
        self.assertEqual(delegated['arguments'], {'task': brief, 'title': 'fix', 'clientRequestId': 'calc.fix'})
        self.assertTrue(all(h.get('authorization') == 'Bearer thread-1'
                            or h.get('Authorization') == 'Bearer thread-1' for h in self.t3.headers))

        # That thread, which T3 Code opens in the lead's folder, is the
        # task's, not the folder's lead: it answers with the task's work.
        child, child_session = self.open(repo)
        answered, updates = child.answer_of(child.ask('session/prompt', {
            'sessionId': child_session, 'prompt': self.wrapped(brief)}))
        self.assertEqual(answered['result']['stopReason'], 'end_turn')
        said = [u for u in updates if u['sessionUpdate'] == 'agent_message_chunk']
        self.assertTrue(said)
        # Its answer, shown whether the task ended before the thread came or
        # after, is a reply a client reacts to.
        reacted, _ = child.answer_of(child.ask('_sonny/react', {
            'sessionId': child_session, 'messageId': said[-1]['messageId'], 'emoji': '👍'}))
        self.assertEqual(reacted.get('result'), {}, reacted)
        kept = self.path / '.agent/sonny/t3/sessions' / child_session
        fix = self.daemon.request('inspect', bot='calc.fix')['result']
        # The thread is the task by its identity and store, not its name alone.
        pinned = json.loads(kept.read_text())
        self.assertEqual((pinned['bot'], pinned['bot_id']), ('calc.fix', fix['bot_id']))
        self.assertRegex(pinned['store'], '^[0-9a-f]{16}$')
        self.assertEqual([b['name'] for b in self.daemon.request('bots')['result']['bots']],
                         ['calc.fix', 'calc.lead'])
        # T3 Code tells the lead's thread its child ended; the lead hears of
        # its task from the lookout, so that message starts no turn.
        answered, _ = thread.answer_of(thread.ask('session/prompt', {'sessionId': session, 'prompt': self.wrapped(
            'Delegated task node-1 reached a terminal state. Use task_status with taskId node-1 to read the result.')}))
        self.assertEqual(answered['result'], {'stopReason': 'end_turn'})
        self.assertEqual(len(self.daemon.request('turns', bot='calc.lead')['result']['turns']), 1)

    def test_a_prompt_that_steers_is_answered_when_the_turn_it_joined_ends(self):
        repo = self.repo()
        self.daemon.request('create', bot='calc.lead', workspace=str(repo), model='openai/synthetic-model',
                            instructions='Lead.', declare=[declared('start_task', inherit=False)],
                            approve=['start_task'], approver='manual')
        thread, session = self.open(repo)
        self.adopt(thread, session)
        self.t3.delegating = False
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
        # Idle before the answer, which waited for the task's thread.
        self.assertIn('idle', [u.get('state') for u in updates if u['sessionUpdate'] == 'state_update'])
        # One turn ran; the second prompt was taken into it.
        turns = self.daemon.request('turns', bot='calc.lead')['result']['turns']
        self.assertEqual([t['status'] for t in turns], ['completed'])
        sent = self.daemon.request('messages', bot='calc.lead')['result']['messages']
        # The opener reads as its turn; the steer as taken into it.
        self.assertEqual([(m['status'], m['turn']) for m in sent],
                         [('completed', turns[0]['turn']), ('taken', turns[0]['turn'])])
        # T3 Code would not take the task as a child: it opens as a thread of
        # its own, in its worktree.
        self.assertEqual(self.t3.calls.get(timeout=30)['name'], 'delegate_task')
        launch = self.t3.calls.get(timeout=30)
        self.assertEqual(launch['name'], 't3_thread_launch')
        self.assertEqual(launch['arguments']['workspaceStrategy'],
                         {'type': 'existing_worktree', 'worktreePath': str(self.path / '.agent/worktrees/calc.fix')})

    def test_a_prompt_cancelled_while_it_waits_in_line_is_answered(self):
        repo = self.repo()
        self.daemon.request('create', bot='calc.lead', workspace=str(repo), model='openai/synthetic-model',
                            instructions='Lead.')
        thread, session = self.open(repo)
        self.adopt(thread, session)
        # Paused, the daemon starts nothing: the prompt waits in line.
        self.assertIn('result', self.daemon.request('pause', reason='rehearsal'))
        prompt = thread.ask('session/prompt', {'sessionId': session, 'prompt': self.wrapped('hello')})
        deadline = time.monotonic() + 10
        while not self.daemon.request('messages', bot='calc.lead')['result']['messages']:
            self.assertLess(time.monotonic(), deadline, 'the prompt never reached the daemon')
            time.sleep(.05)
        thread.send({'method': 'session/cancel', 'params': {'sessionId': session}})
        answered, _ = thread.answer_of(prompt)
        self.assertEqual(answered['result'], {'stopReason': 'cancelled'})
        [sent] = self.daemon.request('messages', bot='calc.lead')['result']['messages']
        self.assertEqual((sent['status'], sent['turn']), ('withdrawn', None))
        self.assertEqual(self.daemon.request('turns', bot='calc.lead')['result']['turns'], [])

    def test_a_turn_the_lead_takes_on_its_own_shows_in_the_thread(self):
        repo = self.repo()
        self.daemon.request('create', bot='calc.lead', workspace=str(repo), model='openai/synthetic-model',
                            instructions='Lead.')
        thread, session = self.open(repo)
        self.adopt(thread, session)
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

    def test_the_model_picked_in_the_thread_is_the_leads(self):
        repo = self.repo()
        (self.path / '.agent').mkdir(exist_ok=True)
        (self.path / '.agent' / 'models').write_text(
            'openai/synthetic-model\nopenai/other-model # faster\nanthropic/claude-x\n')
        # The provider lists a model the file does not name yet.
        self.model.models = ('synthetic-model', 'other-model', 'newer-model')
        self.model.model_names = {'newer-model': 'Newer Model'}
        self.model.context_window = 1_000_000
        thread, session = self.open(repo)
        option, effort = self.opened['result']['configOptions']
        # T3 Code reads `id` even on version 2, which names it `configId`.
        self.assertEqual((option['id'], option['configId'], option['category'], option['currentValue']),
                         ('model', 'model', 'model', 'default'))
        self.assertEqual([c['value'] for c in option['options']],
                         ['default', 'openai/synthetic-model', 'openai/other-model', 'openai/newer-model',
                          'anthropic/claude-x'])
        # Shown by the name the provider lists, else the name Sonny gives
        # it, beside the provider; the full id is in the description.
        self.assertEqual([c['name'] for c in option['options'][1:]],
                         ['Synthetic Model · openai', 'Other Model · openai', 'Newer Model · openai',
                          'X · anthropic'])
        self.assertEqual(option['options'][2]['description'], 'faster (openai/other-model)')
        self.assertEqual(option['options'][3]['description'], 'openai/newer-model')
        self.assertEqual((effort['id'], effort['category'], effort['currentValue']),
                         ('effort', 'thought_level', 'default'))
        self.assertEqual([c['value'] for c in effort['options']],
                         ['default', 'low', 'medium', 'high', 'xhigh'])
        # Picked before there is a lead: the lead Sonny makes runs it.
        picked, _ = thread.answer_of(thread.ask('session/set_config_option', {
            'sessionId': session, 'configId': 'model', 'type': 'id', 'value': 'openai/other-model'}))
        self.assertEqual(picked['result']['configOptions'][0]['currentValue'], 'openai/other-model')
        answered, updates = thread.answer_of(thread.ask('session/prompt', {'sessionId': session,
                                                                            'prompt': self.wrapped('hello')}))
        self.assertEqual(answered.get('result'), {'stopReason': 'end_turn'}, answered)
        lead = self.daemon.request('inspect', bot='calc.lead')['result']
        self.assertEqual((lead['provider'], lead['model']), ('openai', 'other-model'))
        # A lead's thread offers its family's models only.
        [shown] = [u for u in updates if u['sessionUpdate'] == 'config_option_update']
        option, _ = shown['configOptions']
        self.assertEqual(option['currentValue'], 'openai/other-model')
        self.assertEqual([c['value'] for c in option['options']],
                         ['default', 'openai/synthetic-model', 'openai/other-model', 'openai/newer-model'])
        refused, _ = thread.answer_of(thread.ask('session/set_config_option', {
            'sessionId': session, 'configId': 'model', 'type': 'id', 'value': 'anthropic/claude-x'}))
        self.assertEqual(refused['error']['code'], -32602)
        # Picked while the lead is idle: it runs it at once.
        switched, updates = thread.answer_of(thread.ask('session/set_config_option', {
            'sessionId': session, 'configId': 'model', 'type': 'id', 'value': 'openai/synthetic-model'}))
        self.assertEqual(switched['result']['configOptions'][0]['currentValue'], 'openai/synthetic-model')
        self.assertEqual(self.daemon.request('inspect', bot='calc.lead')['result']['model'], 'synthetic-model')
        # An effort the family takes, at once too; one it does not is refused.
        refused, _ = thread.answer_of(thread.ask('session/set_config_option', {
            'sessionId': session, 'configId': 'effort', 'type': 'id', 'value': 'max'}))
        self.assertEqual(refused['error']['code'], -32602)
        raised, _ = thread.answer_of(thread.ask('session/set_config_option', {
            'sessionId': session, 'configId': 'effort', 'type': 'id', 'value': 'high'}))
        self.assertEqual(raised['result']['configOptions'][1]['currentValue'], 'high')
        lead = self.daemon.request('inspect', bot='calc.lead')['result']
        self.assertEqual((lead['model'], lead['effort']), ('synthetic-model', 'high'))
        # Another thread in the folder is a lead of its own, named after its
        # first words, which T3 Code's last pick makes: the lead that is
        # there keeps its own.
        other, again = self.open(repo)
        for option, value in (('model', 'openai/other-model'), ('effort', 'low')):
            other.answer_of(other.ask('session/set_config_option', {
                'sessionId': again, 'configId': option, 'type': 'id', 'value': value}))
        answered, _ = other.answer_of(other.ask('session/prompt', {'sessionId': again,
                                                                    'prompt': self.wrapped('again')}))
        self.assertEqual(answered['result'], {'stopReason': 'end_turn'})
        lead = self.daemon.request('inspect', bot='calc.lead')['result']
        self.assertEqual((lead['model'], lead['effort']), ('synthetic-model', 'high'))
        made = self.daemon.request('inspect', bot='calc.lead.again')['result']
        self.assertEqual((made['model'], made['effort']), ('other-model', 'low'))
        # A thread whose first words are `/switch NAME` is that lead, and one
        # asking for a lead the folder has not is refused.
        third, session = self.open(repo)
        self.adopt(third, session, 'again')
        answered, _ = third.answer_of(third.ask('session/prompt', {'sessionId': session,
                                                                    'prompt': self.wrapped('and more')}))
        self.assertEqual(answered['result'], {'stopReason': 'end_turn'})
        self.assertEqual(len(self.daemon.request('turns', bot='calc.lead.again')['result']['turns']), 2)
        fourth, session = self.open(repo)
        refused, _ = fourth.answer_of(fourth.ask('session/prompt', {'sessionId': session,
                                                                     'prompt': self.wrapped('/switch gone')}))
        self.assertIn('error', refused)
        # Another lead outlives the folder's first.
        self.assertIn('result', self.daemon.request('delete', bot='calc.lead'))
        fifth, session = self.open(repo)
        self.adopt(fifth, session, 'again')

    def test_a_client_picks_the_lead_behind_a_thread_and_lists_the_leads_to_reopen(self):
        """Any client, not only T3 Code: the agent picker names the folder's
        leads, and `session/list` lists them by ids `session/resume`
        takes."""
        repo = self.repo()
        made = {'workspace': str(repo), 'model': 'openai/synthetic-model', 'instructions': 'Lead.'}
        for bot in ('calc.lead', 'calc.lead.billing'):
            self.assertIn('result', self.daemon.request('create', bot=bot, **made))
        env = {k: v for k, v in self.env.items() if not k.startswith('T3_')}
        thread = Thread(self, env, repo)
        thread.answer_of(thread.ask('initialize', {'protocolVersion': 2, 'clientCapabilities': {},
                                                   'clientInfo': {'name': 'slack-bridge'}}))
        opened, _ = thread.answer_of(thread.ask('session/new', {'cwd': str(repo), 'mcpServers': []}))
        session = opened['result']['sessionId']
        picker = opened['result']['configOptions'][-1]
        self.assertEqual((picker['id'], picker['currentValue']), ('agent', 'default'))
        self.assertEqual([c['value'] for c in picker['options']], ['default', 'calc.lead', 'calc.lead.billing'])
        # Picked before the first prompt: that prompt goes to that lead.
        picked, _ = thread.answer_of(thread.ask('session/set_config_option', {
            'sessionId': session, 'configId': 'agent', 'type': 'id', 'value': 'calc.lead.billing'}))
        self.assertEqual(picked['result']['configOptions'][-1]['currentValue'], 'calc.lead.billing')
        hello = [{'type': 'text', 'text': 'hello'}]
        answered, _ = thread.answer_of(thread.ask('session/prompt', {'sessionId': session, 'prompt': hello}))
        self.assertEqual(answered['result'], {'stopReason': 'end_turn'})
        self.assertEqual(len(self.daemon.request('turns', bot='calc.lead.billing')['result']['turns']), 1)
        self.assertEqual(self.daemon.request('turns', bot='calc.lead')['result']['turns'], [])
        # Picked once the thread is a lead's: it moves to the one picked.
        moved, _ = thread.answer_of(thread.ask('session/set_config_option', {
            'sessionId': session, 'configId': 'agent', 'type': 'id', 'value': 'calc.lead'}))
        self.assertEqual(moved['result']['configOptions'][-1]['currentValue'], 'calc.lead')
        answered, _ = thread.answer_of(thread.ask('session/prompt', {'sessionId': session, 'prompt': hello}))
        self.assertEqual(answered['result'], {'stopReason': 'end_turn'})
        self.assertEqual(len(self.daemon.request('turns', bot='calc.lead')['result']['turns']), 1)
        refused, _ = thread.answer_of(thread.ask('session/set_config_option', {
            'sessionId': session, 'configId': 'agent', 'type': 'id', 'value': 'calc.lead.gone'}))
        self.assertEqual(refused['error']['code'], -32602)
        # Listed by folder and in all: each by its name, its identity and its
        # store's lineage.
        ids = {bot: self.daemon.request('inspect', bot=bot)['result']['bot_id']
               for bot in ('calc.lead', 'calc.lead.billing')}
        for params in ({'cwd': str(repo)}, {}):
            listed, _ = thread.answer_of(thread.ask('session/list', params))
            sessions = listed['result']['sessions']
            self.assertEqual([(s['title'], s['cwd']) for s in sessions],
                             [(bot, str(repo)) for bot in ('calc.lead', 'calc.lead.billing')])
            for found in sessions:
                lead, store = found['sessionId'].rsplit('@', 1)
                self.assertEqual(lead, f'{found["title"]}#{ids[found["title"]]}')
                self.assertRegex(store, r'^[0-9a-f]{16}$')
        first, billing = (s['sessionId'] for s in sessions)
        # A listed lead reopens, as that lead even once a session of that id
        # was moved to another.
        reopened = Thread(self, env, repo)
        reopened.answer_of(reopened.ask('initialize', {'protocolVersion': 2, 'clientCapabilities': {}}))
        answered, _ = reopened.answer_of(reopened.ask('session/resume', {'sessionId': first, 'cwd': str(repo),
                                                                          'mcpServers': []}))
        self.assertEqual(answered['result']['configOptions'][-1]['currentValue'], 'calc.lead')
        moved, _ = reopened.answer_of(reopened.ask('session/set_config_option', {
            'sessionId': first, 'configId': 'agent', 'type': 'id', 'value': 'calc.lead.billing'}))
        self.assertEqual(moved['result']['configOptions'][-1]['currentValue'], 'calc.lead.billing')
        reopened.close()
        answered, _ = Thread.resumed(self, env, repo, first)
        self.assertEqual(answered['result']['configOptions'][-1]['currentValue'], 'calc.lead')
        self.assertIn('result', self.resume(repo, billing))
        # One removed since, or of another store, does not.
        lead, store = billing.rsplit('@', 1)
        stale = f'calc.lead.billing#{ids["calc.lead.billing"] + 100}@{store}'
        self.assertIn('error', self.resume(repo, stale))
        other = f'{lead}@{"0" * 16 if store != "0" * 16 else "1" * 16}'
        self.assertIn('another store', self.resume(repo, other)['error']['message'])

    def test_a_task_the_lead_starts_is_a_thread_a_client_opens(self):
        """Any client, not only T3 Code: a task the lead starts comes with
        the session id that opens it, `session/list` lists it under its
        lead, and `session/resume` opens it, as it opens no other bot."""
        repo = self.repo()
        self.daemon.request('create', bot='calc.lead', workspace=str(repo), model='openai/synthetic-model',
                            instructions='Lead.', declare=[declared('start_task', inherit=False)])
        env = {k: v for k, v in self.env.items() if not k.startswith('T3_')}
        thread = Thread(self, env, repo)
        thread.answer_of(thread.ask('initialize', {'protocolVersion': 2, 'clientCapabilities': {},
                                                   'clientInfo': {'name': 'slack-bridge'}}))
        opened, _ = thread.answer_of(thread.ask('session/new', {'cwd': str(repo), 'mcpServers': []}))
        session = opened['result']['sessionId']
        thread.answer_of(thread.ask('session/set_config_option', {
            'sessionId': session, 'configId': 'agent', 'type': 'id', 'value': 'calc.lead'}))
        self.model.call_script = [('start_task', {'name': 'fix', 'brief': 'Fix calc.py.'})]
        answered, updates = thread.answer_of(thread.ask('session/prompt', {
            'sessionId': session, 'prompt': [{'type': 'text', 'text': 'script'}]}))
        self.assertEqual(answered['result'], {'stopReason': 'end_turn'})
        fix = self.daemon.request('inspect', bot='calc.fix')['result']
        lead_id = self.daemon.request('inspect', bot='calc.lead')['result']['bot_id']
        # The call that started it names it, before the prompt is answered.
        [task] = [u['_meta']['task'] for u in updates
                  if u['sessionUpdate'] == 'tool_call_update' and 'task' in u.get('_meta', {})]
        name, store = task['sessionId'].rsplit('@', 1)
        self.assertEqual(name, f'calc.fix#{fix["bot_id"]}')
        self.assertRegex(store, r'^[0-9a-f]{16}$')
        self.assertEqual((task['title'], task['cwd']), ('calc.fix', fix['workspace']))
        # Listed by folder under its lead, which it names; in all, leads only.
        listed, _ = thread.answer_of(thread.ask('session/list', {'cwd': str(repo)}))
        sessions = listed['result']['sessions']
        self.assertEqual([(s['title'], s.get('_meta')) for s in sessions],
                         [('calc.lead', None), ('calc.fix', {'lead': f'calc.lead#{lead_id}@{store}'})])
        self.assertEqual((sessions[1]['sessionId'], sessions[1]['cwd']), (task['sessionId'], fix['workspace']))
        listed, _ = thread.answer_of(thread.ask('session/list', {}))
        self.assertEqual([s['title'] for s in listed['result']['sessions']], ['calc.lead'])
        # It opens as the task, and a prompt there is the task's.
        reopened = Thread(self, env, repo)
        reopened.answer_of(reopened.ask('initialize', {'protocolVersion': 2, 'clientCapabilities': {}}))
        answered, _ = reopened.answer_of(reopened.ask('session/resume', {
            'sessionId': task['sessionId'], 'cwd': str(repo), 'mcpServers': []}))
        self.assertEqual(answered['result']['configOptions'][-1]['currentValue'], 'calc.fix')
        before = len(self.daemon.request('turns', bot='calc.fix')['result']['turns'])
        answered, _ = reopened.answer_of(reopened.ask('session/prompt', {
            'sessionId': task['sessionId'], 'prompt': [{'type': 'text', 'text': 'how is it going?'}]}))
        self.assertEqual(answered['result'], {'stopReason': 'end_turn'})
        self.assertEqual(len(self.daemon.request('turns', bot='calc.fix')['result']['turns']), before + 1)
        reopened.close()
        # A bot no lead started does not open, by any id.
        self.daemon.request('create', bot='calc.loose', workspace=str(repo), model='openai/synthetic-model',
                            instructions='Loose.')
        loose = self.daemon.request('inspect', bot='calc.loose')['result']['bot_id']
        refused = self.resume(repo, f'calc.loose#{loose}@{store}')
        self.assertIn('does not know', refused['error']['message'])

    def test_a_client_names_who_wrote_each_prompt_and_who_answered_for_a_call(self):
        """A client several people write through, as a team's chat bridge:
        `_meta.person` names who wrote a prompt, which the lead reads, and
        who chose Allow or Deny, which the daemon keeps."""
        repo = self.repo()
        self.daemon.request('create', bot='calc.lead', workspace=str(repo), model='openai/synthetic-model',
                            instructions='Lead.', declare=[declared('start_task', inherit=False)],
                            approve=['start_task'], approver='manual')
        env = {k: v for k, v in self.env.items() if not k.startswith('T3_')}
        thread = Thread(self, env, repo)
        thread.answer_of(thread.ask('initialize', {'protocolVersion': 2, 'clientCapabilities': {},
                                                   'clientInfo': {'name': 'slack-bridge'}}))
        opened, _ = thread.answer_of(thread.ask('session/new', {'cwd': str(repo), 'mcpServers': []}))
        session = opened['result']['sessionId']
        thread.answer_of(thread.ask('session/set_config_option', {
            'sessionId': session, 'configId': 'agent', 'type': 'id', 'value': 'calc.lead'}))
        # A name the daemon does not take is refused before anything is sent.
        refused, _ = thread.answer_of(thread.ask('session/prompt', {
            'sessionId': session, 'prompt': [{'type': 'text', 'text': 'hi'}], '_meta': {'person': 'Ana Lopez'}}))
        self.assertEqual(refused['error']['code'], -32602)
        self.assertEqual(self.daemon.request('turns', bot='calc.lead')['result']['turns'], [])
        self.model.call_script = [('start_task', {'name': 'fix', 'brief': 'Fix calc.py.'})]
        prompt = thread.ask('session/prompt', {'sessionId': session, 'prompt': [{'type': 'text', 'text': 'script'}],
                                               '_meta': {'person': 'ana'}})
        [asked] = [m for m in thread.until(lambda m: m.get('method') == 'session/request_permission')
                   if m.get('method') == 'session/request_permission']
        # The model read who wrote it, after the words and their time.
        content = self.model.requests.get(timeout=5)['input'][-1]['content']
        self.assertRegex(content[0]['text'], SENT)
        self.assertTrue(content[0]['text'].startswith('script\n\n'))
        self.assertRegex(content[1]['text'], r'^\[Message calc\.lead/\d+, written by ana\.\]$')
        thread.send({'id': asked['id'], 'result': {'outcome': {'outcome': 'selected', 'optionId': 'deny'},
                                                   '_meta': {'person': 'bo'}}})
        answered, _ = thread.answer_of(prompt)
        self.assertEqual(answered['result'], {'stopReason': 'end_turn'})
        events = self.daemon.request('events', bot='calc.lead', after=0, limit=256)['result']['events']
        [denied] = [e['data'] for e in events if e['event'] == 'tool_completed']
        self.assertEqual([(v['by'], v['allow']) for v in denied['approvals']], [('bo', False)])

    def test_reactions_go_both_ways_between_the_lead_and_a_client(self):
        """The lead's own reaction reaches the client as `_sonny/react`; a
        client's reaction to one of the lead's replies is attached to the
        lead's next model call, as the person's in Sonny's window, or as the
        words of the person the client names."""
        repo = self.repo()
        self.daemon.request('create', bot='calc.lead', workspace=str(repo), model='openai/synthetic-model',
                            instructions='Lead.', declare=[declared('react')])
        self.daemon.request('create', bot='calc.lead.billing', workspace=str(repo),
                            model='openai/synthetic-model', instructions='Lead.')
        env = {k: v for k, v in self.env.items() if not k.startswith('T3_')}
        thread = Thread(self, env, repo)
        thread.answer_of(thread.ask('initialize', {'protocolVersion': 2, 'clientCapabilities': {},
                                                   'clientInfo': {'name': 'slack-bridge'}}))
        opened, _ = thread.answer_of(thread.ask('session/new', {'cwd': str(repo), 'mcpServers': []}))
        session = opened['result']['sessionId']
        thread.answer_of(thread.ask('session/set_config_option', {
            'sessionId': session, 'configId': 'agent', 'type': 'id', 'value': 'calc.lead'}))
        script = [{'type': 'text', 'text': 'script'}]
        self.model.call_script = [('react', {'emoji': '👀'})]
        prompt = thread.ask('session/prompt', {'sessionId': session, 'prompt': script})
        seen = thread.until(lambda m: m.get('id') == prompt and 'method' not in m)
        self.assertEqual(seen[-1].get('result'), {'stopReason': 'end_turn'}, seen[-1])
        self.assertEqual([m['params'] for m in seen if m.get('method') == '_sonny/react'],
                         [{'sessionId': session, 'emoji': '👀', 'remove': False}])

        def reply(seen):
            [said] = {m['params']['update']['messageId'] for m in seen if m.get('method') == 'session/update'
                      and m['params']['update']['sessionUpdate'] == 'agent_message_chunk'}
            return said
        older = reply(seen)
        again = thread.ask('session/prompt', {'sessionId': session, 'prompt': script})
        last = reply(thread.until(lambda m: m.get('id') == again and 'method' not in m))

        def react(params):
            answered, _ = thread.answer_of(thread.ask('_sonny/react', {'sessionId': session, **params}))
            return answered
        self.assertEqual(react({'messageId': 'calc.lead-0-9', 'emoji': '🎉'})['error']['code'], -32602)
        self.assertEqual(react({'messageId': older, 'emoji': 'nice'})['error']['code'], -32602)
        self.assertEqual(react({'messageId': older, 'emoji': '🎉'})['result'], {})
        # The person's own is kept where Sonny's window draws it, under that
        # reply; one again is the same one, and is not sent twice.
        kept = self.path / '.agent/sonny/reactions'
        [file] = [f for f in kept.iterdir() if f.name.endswith('.json') and not f.name.endswith('.by.json')]
        self.assertEqual(list(json.loads(file.read_text()).values()), [['🎉']])
        self.assertIn('result', react({'messageId': older, 'emoji': '🎉'}))
        self.assertIn('result', react({'messageId': last, 'emoji': '👍', '_meta': {'person': 'ana'}}))
        self.assertEqual(list(json.loads(file.read_text()).values()), [['🎉']])
        while not self.model.requests.empty():
            self.model.requests.get_nowait()
        hello = [{'type': 'text', 'text': 'hello'}]
        answered, _ = thread.answer_of(thread.ask('session/prompt', {'sessionId': session, 'prompt': hello}))
        self.assertEqual(answered['result'], {'stopReason': 'end_turn'})
        said = [part['text'] for item in self.model.requests.get(timeout=5)['input'] if item.get('role') == 'user'
                for part in item['content'] if part.get('type') == 'input_text']
        # The older reply quoted; the last, which a teammate reacted to, not.
        self.assertIn('> done\n\n🎉', said)
        self.assertEqual(said.count('> done\n\n🎉'), 1)
        self.assertIn('👍', said)
        self.assertRegex(said[said.index('👍') + 1], r'^\[Message calc\.lead/\d+, written by ana\.\]$')
        # Taken off: off in the window too, and the lead is not told.
        self.assertIn('result', react({'messageId': older, 'emoji': '🎉', 'remove': True}))
        self.assertEqual(json.loads(file.read_text()), {})
        # Moved to another lead: the first lead's replies are not its.
        moved, _ = thread.answer_of(thread.ask('session/set_config_option', {
            'sessionId': session, 'configId': 'agent', 'type': 'id', 'value': 'calc.lead.billing'}))
        self.assertEqual(moved['result']['configOptions'][-1]['currentValue'], 'calc.lead.billing')
        self.assertEqual(react({'messageId': last, 'emoji': '🎉'})['error']['code'], -32602)

    def test_the_leads_questions_are_forms_and_a_pick_answers_them(self):
        """A client that fills in forms gets each question the lead puts,
        live or still open from before it followed the lead; a pick goes
        to the lead as the person's answer, or as the words of the person
        the client names."""
        repo = self.repo()
        self.daemon.request('create', bot='calc.lead', workspace=str(repo), model='openai/synthetic-model',
                            instructions='Lead.')
        env = {k: v for k, v in self.env.items() if not k.startswith('T3_')}

        def asked(question, first, second):
            return ('ask', {'question': question, 'why': 'It decides the schema.',
                            'options': [{'label': first, 'explanation': f'{first} it is.'},
                                        {'label': second, 'explanation': f'{second} it is.'}],
                            'recommended': 1, 'reason': 'Simplest.', 'if_unanswered': 'wait'})

        self.model.call_script = [[asked('Which database?', 'SQLite', 'Postgres'),
                                   asked('Which cache?', 'None', 'Redis')]]
        # Asked while no client that fills in forms follows the lead.
        thread = Thread(self, env, repo)
        thread.answer_of(thread.ask('initialize', {'protocolVersion': 2, 'clientCapabilities': {}}))
        opened, _ = thread.answer_of(thread.ask('session/new', {'cwd': str(repo), 'mcpServers': []}))
        session = opened['result']['sessionId']
        prompt = thread.ask('session/prompt', {'sessionId': session, 'prompt': [{'type': 'text', 'text': 'script'}]})
        seen = thread.until(lambda m: m.get('id') == prompt and 'method' not in m)
        self.assertEqual(seen[-1].get('result'), {'stopReason': 'end_turn'}, seen[-1])
        self.assertNotIn('elicitation/create', [m.get('method') for m in seen])
        thread.close()
        # A client that does, reopening the thread: both are put to it, the
        # oldest first.
        thread = Thread(self, env, repo)
        thread.answer_of(thread.ask('initialize', {'protocolVersion': 2,
                                                   'clientCapabilities': {'elicitation': {'form': {}}}}))
        thread.answer_of(thread.ask('session/resume', {'sessionId': session, 'cwd': str(repo), 'mcpServers': []}))
        forms = []

        def both(message):
            if message.get('method') == 'elicitation/create':
                forms.append(message)
            return len(forms) == 2
        thread.until(both)
        self.assertEqual([f['params']['message'].split('\n')[0] for f in forms], ['Which database?', 'Which cache?'])
        database = forms[0]['params']
        self.assertEqual(database['mode'], 'form')
        self.assertEqual(database['message'],
                         'Which database?\n\nIt decides the schema.\n\nRecommended: SQLite. Simplest.')
        self.assertNotIn('toolCallId', database)
        pick = database['requestedSchema']['properties']['pick']
        self.assertEqual(database['requestedSchema']['required'], ['pick'])
        self.assertEqual(pick['oneOf'], [{'const': '1', 'title': 'SQLite', 'description': 'SQLite it is.'},
                                         {'const': '2', 'title': 'Postgres', 'description': 'Postgres it is.'}])
        # The person picks for the first; a teammate the client names for
        # the second.
        thread.send({'id': forms[0]['id'], 'result': {'action': 'accept', 'content': {'pick': '2'}}})
        thread.send({'id': forms[1]['id'], 'result': {'action': 'accept', 'content': {'pick': '1'},
                                                      '_meta': {'person': 'ana'}}})
        sent = []
        deadline = time.monotonic() + 30
        while not ('Postgres' in ''.join(sent) and 'written by ana' in ''.join(sent)):
            self.assertLess(time.monotonic(), deadline, sent)
            try:
                sent.append(json.dumps(self.model.requests.get(timeout=1)['input']))
            except queue.Empty:
                pass
        texts = {part['text'] for request in sent for item in json.loads(request)
                 if item.get('role') == 'user' and isinstance(item.get('content'), list)
                 for part in item['content'] if 'text' in part}
        database_answer = next(t for t in texts if t.startswith('You asked (') and 'Which database?' in t)
        self.assertRegex(database_answer, r'^You asked \(ask:\d+/script-0\): Which database\?\n\nPostgres\n\n\[Sent ')
        cache_answer = next(t for t in texts if t.startswith('You asked (') and 'Which cache?' in t)
        self.assertRegex(cache_answer, r'^You asked \(ask:\d+/script-0\.1\): Which cache\?\n\nNone\n\n\[Sent ')
        self.assertTrue(any(re.fullmatch(r'\[Message calc\.lead\.script/\d+, written by ana\.\]', t)
                            for t in texts), texts)
        # Once the person's answers are in, a question asked live comes as a
        # form on the call that put it; declined, it waits on.
        deadline = time.monotonic() + 30
        while any(m['status'] in ('running', 'queued', 'ready') for m in
                  self.daemon.request('messages', bot='calc.lead')['result']['messages']):
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.05)
        self.model.call_script.append(asked('Which queue?', 'None', 'SQS'))
        thread.ask('session/prompt', {'sessionId': session, 'prompt': [{'type': 'text', 'text': 'script'}]})
        [live] = [m for m in thread.until(lambda m: m.get('method') == 'elicitation/create')
                  if m.get('method') == 'elicitation/create']
        self.assertEqual(live['params']['message'].split('\n')[0], 'Which queue?')
        self.assertEqual(live['params']['toolCallId'], 'script-1')
        # A pick naming someone by a name the daemon would not take answers
        # nothing, and is nobody's words.
        thread.send({'id': live['id'], 'result': {'action': 'accept', 'content': {'pick': '2'},
                                                  '_meta': {'person': 'Ana Lopez'}}})
        hello = thread.ask('session/prompt', {'sessionId': session, 'prompt': [{'type': 'text', 'text': 'hello'}]})
        thread.until(lambda m: m.get('id') == hello and 'method' not in m)
        later = []
        while True:
            try:
                later.append(json.dumps(self.model.requests.get_nowait()['input']))
            except queue.Empty:
                break
        self.assertTrue(any('hello' in request for request in later))
        self.assertFalse(any('script-1): Which queue' in request for request in later))

    def test_the_tasks_questions_come_to_the_leads_thread_as_forms(self):
        """As Sonny's lead window pins them: a client that fills in forms,
        on a lead's thread, gets each question the lead's tasks have open,
        from before it came, asked live, and from a task the lead starts
        since, each saying whose it is; a pick answers it in that task."""
        repo = self.repo()
        made = {'workspace': str(repo), 'model': 'openai/synthetic-model'}
        self.daemon.request('create', bot='calc.lead', instructions='Lead.',
                            declare=[declared('start_task', inherit=False)], **made)
        lead_id = self.daemon.request('inspect', bot='calc.lead')['result']['bot_id']
        self.assertIn('result', self.daemon.request(
            'create', bot='calc.fix', instructions='Task.', declare=[declared('ask', inherit=False)],
            created_by='calc.lead', created_by_id=lead_id, **made))
        env = {k: v for k, v in self.env.items() if not k.startswith('T3_')}

        def asked(question, first, second):
            return ('ask', {'question': question, 'why': 'It decides the schema.',
                            'options': [{'label': first, 'explanation': ''}, {'label': second, 'explanation': ''}],
                            'if_unanswered': 'wait'})

        def ran(bot, prompt):
            deadline = time.monotonic() + 30
            while self.daemon.request('inspect', bot=bot)['result']['running_turn'] is not None:
                self.assertLess(time.monotonic(), deadline)
                time.sleep(.05)
            submitted = self.daemon.request('submit', bot=bot, prompt=prompt, request_id=f'{bot}-{time.monotonic()}')
            turn = submitted['result']['message']
            deadline = time.monotonic() + 30
            while self.daemon.request('inspect', bot=bot)['result']['running_turn'] == turn:
                self.assertLess(time.monotonic(), deadline)
                time.sleep(.05)

        def answers(bot):
            turns = self.daemon.request('turns', bot=bot)['result']['turns']
            return [p['text'] for t in turns
                    for p in self.daemon.request('prompts', bot=bot, turn=t['turn'])['result']['prompts']
                    if p['text'].startswith('You asked (')]

        def until_answered(bot, count):
            deadline = time.monotonic() + 30
            while len(answers(bot)) < count:
                self.assertLess(time.monotonic(), deadline, answers(bot))
                time.sleep(.05)
            return answers(bot)

        def form(thread):
            [put] = [m for m in thread.until(lambda m: m.get('method') == 'elicitation/create')
                     if m.get('method') == 'elicitation/create']
            return put

        # Asked while no client that fills in forms follows the lead (one
        # that does not has Sonny's tools served).
        thread = Thread(self, env, repo)
        thread.answer_of(thread.ask('initialize', {'protocolVersion': 2, 'clientCapabilities': {}}))
        listed, _ = thread.answer_of(thread.ask('session/list', {'cwd': str(repo)}))
        self.assertIn('result', listed, listed)
        lead = listed['result']['sessions'][0]['sessionId']
        thread.answer_of(thread.ask('session/resume', {'sessionId': lead, 'cwd': str(repo), 'mcpServers': []}))
        self.model.call_script = [[asked('Which database?', 'SQLite', 'Postgres'), asked('Which cache?', 'None', 'Redis')]]
        ran('calc.fix', 'script')
        thread.close()
        thread = Thread(self, env, repo)
        thread.answer_of(thread.ask('initialize', {'protocolVersion': 2,
                                                   'clientCapabilities': {'elicitation': {'form': {}}}}))
        thread.answer_of(thread.ask('session/resume', {'sessionId': lead, 'cwd': str(repo), 'mcpServers': []}))
        # Each it has open, the oldest first.
        database, cache = form(thread), form(thread)
        self.assertEqual(database['params']['message'], 'From calc.fix\n\nWhich database?\n\nIt decides the schema.')
        self.assertTrue(cache['params']['message'].startswith('From calc.fix\n\nWhich cache?'))
        self.assertNotIn('toolCallId', database['params'])
        thread.send({'id': database['id'], 'result': {'action': 'accept', 'content': {'pick': '2'}}})
        [answer] = until_answered('calc.fix', 1)
        self.assertRegex(answer, r'^You asked \(ask:\d+/script-0\): Which database\?\n\nPostgres\n\n\[Sent ')
        thread.send({'id': cache['id'], 'result': {'action': 'accept', 'content': {'pick': '1'}}})
        self.assertIn('Which cache?\n\nNone', until_answered('calc.fix', 2)[-1])
        self.assertEqual(self.daemon.request('turns', bot='calc.lead')['result']['turns'], [])
        # Asked live.
        self.model.call_script.append(asked('Which queue?', 'None', 'SQS'))
        ran('calc.fix', 'script')
        queue_form = form(thread)
        self.assertTrue(queue_form['params']['message'].startswith('From calc.fix\n\nWhich queue?'))
        thread.send({'id': queue_form['id'], 'result': {'action': 'accept', 'content': {'pick': '1'}}})
        self.assertIn('Which queue?\n\nNone', until_answered('calc.fix', 3)[-1])
        # Asked by a task the lead started after the thread came.
        self.model.call_script = [('start_task', {'name': 'check', 'brief': 'Check calc.py.', 'reason': 'A check apart from the fix.'})]
        answered, updates = thread.answer_of(thread.ask('session/prompt', {
            'sessionId': lead, 'prompt': [{'type': 'text', 'text': 'script'}]}))
        self.assertEqual(answered['result'], {'stopReason': 'end_turn'})
        self.assertIn('calc.check', [b['name'] for b in self.daemon.request('bots')['result']['bots']], updates)
        self.model.call_script = [asked('Which colour?', 'Red', 'Blue')]
        ran('calc.check', 'script')
        colour = form(thread)
        self.assertTrue(colour['params']['message'].startswith('From calc.check\n\nWhich colour?'))
        # Another bot made under a task's name, after it is gone, is not
        # that task: its questions are its own.
        self.assertIn('result', self.daemon.request('delete', bot='calc.fix'))
        self.assertIn('result', self.daemon.request(
            'create', bot='calc.fix', instructions='Not a task.', declare=[declared('ask', inherit=False)], **made))
        self.model.call_script = [asked('Which secret?', 'One', 'Two')]
        ran('calc.fix', 'script')
        hello = thread.ask('session/prompt', {'sessionId': lead, 'prompt': [{'type': 'text', 'text': 'hello'}]})
        seen = thread.until(lambda m: m.get('id') == hello and 'method' not in m)
        self.assertNotIn('elicitation/create', [m.get('method') for m in seen])

    def test_t3_code_threads_pick_their_lead_by_words_not_by_an_option(self):
        """T3 Code carries option picks over to the next thread it opens."""
        thread, _ = self.open(self.repo())
        self.assertEqual([o['id'] for o in self.opened['result']['configOptions']], ['model', 'effort'])

    def resume(self, cwd, session):
        thread = Thread(self, self.env, cwd)
        thread.answer_of(thread.ask('initialize', {'protocolVersion': 2, 'clientCapabilities': {}}))
        answered, _ = thread.answer_of(thread.ask('session/resume', {'sessionId': session, 'cwd': str(cwd),
                                                                      'mcpServers': []}))
        thread.close()
        return answered

    def test_a_thread_comes_back_as_its_bot_and_not_another_made_under_its_name(self):
        repo = self.repo()
        made = {'workspace': str(repo), 'model': 'openai/synthetic-model', 'instructions': 'Lead.'}
        self.daemon.request('create', bot='calc.lead', **made)
        thread, session = self.open(repo)
        self.adopt(thread, session)
        thread.close()
        self.assertIn('result', self.resume(repo, session))
        # The lead removed and another made under its name: the thread is
        # neither, and is refused rather than bound to the new one.
        self.assertIn('result', self.daemon.request('delete', bot='calc.lead'))
        deadline = time.monotonic() + 10
        while 'error' in self.daemon.request('create', bot='calc.lead', **made):
            self.assertLess(time.monotonic(), deadline)
            time.sleep(0.05)
        refused = self.resume(repo, session)
        self.assertIn('calc.lead is gone', refused['error']['message'])

    def test_a_thread_sonny_does_not_know_does_not_come_back(self):
        repo = self.repo()
        thread = Thread(self, self.env, repo)
        thread.answer_of(thread.ask('initialize', {'protocolVersion': 2, 'clientCapabilities': {}}))
        refused, _ = thread.answer_of(thread.ask('session/resume', {'sessionId': 'sonny-gone', 'cwd': str(repo),
                                                                     'mcpServers': []}))
        self.assertIn('sonny-gone', refused['error']['message'])
        self.assertEqual(self.daemon.request('bots')['result']['bots'], [])
