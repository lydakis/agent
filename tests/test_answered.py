"""Answered declared tools: a call waits for its client's answer, live or parked."""
import json
import os
import time
import unittest

from bench.runtime_client import node_item
from bench.socket_client import Connection, SocketClient
from tests.test_runtime import ModelFixture

START = {'name': 'start_task', 'description': 'Start a task.',
         'parameters': {'type': 'object', 'properties': {'name': {'type': 'string'}},
                        'required': ['name'], 'additionalProperties': False},
         'answered_by': 'sonny'}
# A script whose nested calls its client puts to the bot's gates one by one.
EXEC = {'name': 'exec', 'description': 'Run a script.',
        'parameters': {'type': 'object', 'properties': {'code': {'type': 'string'}},
                       'required': ['code'], 'additionalProperties': False},
        'answered_by': 'sonny', 'steps': True}


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class AnsweredToolTests(ModelFixture):
    def lead(self, settings=None, tool=None, client=None, **create):
        client = client or self.client('echo', settings=settings)
        created = client.request('create', bot='Lead', workspace=str(self.path),
                                 declare=[tool or START], **create)['result']
        self.assertEqual(created['declared'][0]['answered_by'], 'sonny')
        self.model.call_script = [('start_task', {'name': 'fix'})]
        return client

    def submit(self, client):
        return client.request('submit', bot='Lead', request_id='t', prompt='script')['result']['turn']

    def started(self, client, turn):
        return client.receive(lambda m: m.get('event') == 'tool_started' and m.get('turn') == turn)['data']

    def answer(self, client, turn, **answer):
        return client.request('tool_result', bot=answer.pop('bot', 'Lead'), turn=turn,
                              call_id=answer.pop('call_id', 'script-0'), **answer)

    def output(self, client, call_id='script-0'):
        events = client.request('events', bot='Lead', after=0, limit=256)['result']['events']
        done = [e for e in events if e['event'] == 'tool_completed' and e['data']['call_id'] == call_id][-1]
        return node_item(client, 'Lead', done['data']['node'])['result']['output']

    def events(self, client, turn, kind):
        events = client.request('events', bot='Lead', after=0, limit=256)['result']['events']
        return [e['data'] for e in events if e.get('turn') == turn and e['event'] == kind]

    def test_a_declared_tool_names_its_answerer_only_as_a_tag_with_a_sane_expiry(self):
        client = self.client('echo')
        for tool, detail in [(dict(START, result='Started.'), 'result'),
                             (dict(START, answered_by='a b'), 'tag'),
                             (dict(START, answered_by=''), 'empty'),
                             (dict(START, expire_ms=0), 'zero'),
                             (dict(START, expire_ms=3_600_001), 'long'),
                             ({k: v for k, v in START.items() if k != 'answered_by'} | {'expire_ms': 5}, 'alone')]:
            refused = client.request('create', bot='Bad', workspace=str(self.path), declare=[tool])
            self.assertEqual(refused['error'], 'invalid_declare', detail)

    def test_a_tool_with_a_grammar_takes_free_text_and_is_answered_as_custom(self):
        client = self.client('echo')
        exec_tool = {'name': 'exec', 'description': 'Run a script.', 'grammar': 'start: /[\\s\\S]+/',
                     'parameters': {'type': 'object', 'properties': {'input': {'type': 'string'}},
                                    'required': ['input']},
                     'answered_by': 'sonny'}
        for parameters in ({'type': 'object'}, {'type': 'object', 'properties': {'input': {'type': 'string'}}},
                           {'type': 'object', 'properties': {'input': {'type': 'integer'}}, 'required': ['input']},
                           {'type': 'object', 'properties': {'input': {'type': 'string'}, 'language': {'type': 'string'}},
                            'required': ['input', 'language']}):
            refused = client.request('create', bot='Bad', workspace=str(self.path),
                                     declare=[dict(exec_tool, parameters=parameters)])
            self.assertEqual(refused['error'], 'invalid_declare', parameters)
        refused = client.request('create', bot='Bad', workspace=str(self.path), declare=[dict(exec_tool, grammar='')])
        self.assertEqual(refused['error'], 'invalid_declare')
        client = self.lead(tool=exec_tool, client=client)
        self.model.call_script = [('exec', 'text("hi")')]
        turn = self.submit(client)
        self.started(client, turn)
        [call] = client.request('tool_calls', tag='sonny')['result']['calls']
        self.assertEqual((call['name'], call['arguments']), ('exec', {'input': 'text("hi")'}))
        self.answer(client, turn, output='hi')
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        requests = []
        while not self.model.requests.empty():
            requests.append(self.model.requests.get())
        [declared] = [t for t in requests[0]['tools'] if t['name'] == 'exec']
        self.assertEqual((declared['type'], declared['format']['definition']), ('custom', 'start: /[\\s\\S]+/'))
        [answered] = [i for i in requests[-1]['input'] if i.get('call_id') == 'script-0' and 'output' in i]
        self.assertEqual((answered['type'], answered['output']), ('custom_tool_call_output', 'hi'))

    def test_an_answer_within_the_hold_is_the_result_and_the_turn_never_parks(self):
        client = self.lead()
        turn = self.submit(client)
        started = self.started(client, turn)
        self.assertEqual(started['answered_by'], 'sonny')
        self.assertGreater(started['expires_ms'], time.time() * 1000)
        [call] = client.request('tool_calls', tag='sonny')['result']['calls']
        self.assertEqual({k: call[k] for k in ('bot', 'turn', 'call_id', 'name', 'arguments')},
                         {'bot': 'Lead', 'turn': turn, 'call_id': 'script-0', 'name': 'start_task',
                          'arguments': {'name': 'fix'}})
        self.assertEqual(client.request('tool_calls', tag='other')['result'], {'calls': [], 'next_after': None})
        self.assertEqual(self.answer(client, turn, output='a', error={'code': 'x'})['error'], 'invalid_tool_result')
        self.assertEqual(self.answer(client, turn, error={'code': 'Not Ok'})['error'], 'invalid_tool_result')
        self.assertEqual(self.answer(client, turn, output='x' * 65537)['error'], 'tool_output_limit')
        self.assertEqual(self.answer(client, turn, call_id='nope', output='a')['error'], 'no_pending_call')
        self.assertEqual(self.answer(client, turn, bot='Other', output='a')['error'], 'turn_not_found')
        self.assertEqual(self.answer(client, turn, output='started fix')['result'],
                         {'bot': 'Lead', 'turn': turn, 'call_id': 'script-0'})
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertEqual(self.output(client), 'started fix')
        self.assertEqual(self.events(client, turn, 'turn_waiting'), [])
        self.assertEqual(client.request('tool_calls', tag='sonny')['result']['calls'], [])
        self.assertEqual(self.answer(client, turn, output='again')['error'], 'stale_turn')

    def test_a_slow_answer_parks_the_turn_and_its_error_is_the_result(self):
        client = self.lead(settings={'approval_hold_ms': 0})
        turn = self.submit(client)
        waiting = client.receive(lambda m: m.get('event') == 'turn_waiting' and m.get('turn') == turn)['data']
        self.assertEqual((waiting['call_id'], waiting['answer']), ('script-0', 'sonny'))
        stats = client.request('stats')['result']
        self.assertEqual((stats['active_turns'], stats['waiting_turns']), (0, 1))
        [view] = client.request('turns', bot='Lead', newest=True, limit=1)['result']['turns']
        self.assertEqual(view['waiting_on'], {'call_id': 'script-0', 'answer': 'sonny',
                                              'deadline_ms': waiting['deadline_ms']})
        self.assertEqual(len(client.request('tool_calls', tag='sonny')['result']['calls']), 1)
        answered = self.answer(client, turn, error={'code': 'task_exists', 'detail': 'fix exists'})
        self.assertIn('result', answered)
        self.assertEqual(self.answer(client, turn, output='twice')['error'], 'call_already_answered')
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertEqual(json.loads(self.output(client)), {'error': 'task_exists', 'detail': 'fix exists'})
        self.assertEqual(len(self.events(client, turn, 'turn_resumed')), 1)

    def test_a_call_nobody_answers_in_time_is_unserved_and_the_turn_goes_on(self):
        # Out of time within the hold, and while parked.
        for expire_ms, parks in ((100, False), (600, True)):
            with self.subTest(expire_ms=expire_ms):
                self.setUp()
                client = self.lead(tool=dict(START, expire_ms=expire_ms))
                turn = self.submit(client)
                self.assertEqual(client.finished(turn)['data']['status'], 'completed')
                output = json.loads(self.output(client))
                self.assertEqual(output['error'], 'tool_unserved')
                self.assertEqual(bool(self.events(client, turn, 'turn_waiting')), parks)
                self.assertEqual(self.answer(client, turn, output='late')['error'], 'stale_turn')

    def test_a_parked_call_survives_restart_and_interrupt_ends_it(self):
        client = self.lead(settings={'approval_hold_ms': 0})
        turn = self.submit(client)
        client.receive(lambda m: m.get('event') == 'turn_waiting' and m.get('turn') == turn)
        client.close(kill=True)
        client = self.client('echo')
        self.assertEqual(len(client.request('tool_calls', tag='sonny')['result']['calls']), 1)
        self.answer(client, turn, output='after restart')
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertEqual(self.output(client), 'after restart')
        # A second turn parks, and an interrupt ends it with the call.
        self.model.call_script = [None, ('start_task', {'name': 'next'})]
        turn = client.request('submit', bot='Lead', request_id='u', prompt='script')['result']['turn']
        client.receive(lambda m: m.get('event') == 'turn_waiting' and m.get('turn') == turn)
        self.assertEqual(client.request('interrupt', bot='Lead', turn=turn)['result']['status'], 'interrupted')
        self.assertEqual(client.request('tool_calls', tag='sonny')['result']['calls'], [])
        self.assertEqual(self.answer(client, turn, call_id='script-1', output='late')['error'], 'stale_turn')

    def test_a_gate_holds_back_a_declared_tool_before_its_answer(self):
        client = self.client('echo')
        refused = client.request('create', bot='Bad', workspace=str(self.path), declare=[START],
                                 approve=['react'], approver='manual')
        self.assertEqual(refused['error'], 'approve_not_in_tools')
        client = self.lead(client=client, approve=['start_task'], approver='manual')
        turn = self.submit(client)
        [call] = client.receive(lambda m: m.get('event') == 'approval_requested'
                                and m.get('turn') == turn)['data']['calls']
        self.assertEqual(call['name'], 'start_task')
        # Not waiting for an answer until it is allowed and starts.
        self.assertEqual(client.request('tool_calls', tag='sonny')['result']['calls'], [])
        client.request('answer', bot='Lead', turn=turn, call_id='script-0', request=1, decision='allow', by='test')
        self.assertEqual(self.started(client, turn)['answered_by'], 'sonny')
        self.answer(client, turn, output='allowed and started')
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertEqual(self.output(client), 'allowed and started')

    def test_a_later_calls_gate_that_lapses_ends_the_wait_for_an_answer(self):
        client = self.client('echo,shell')
        self.lead(client=client, tool=dict(START, expire_ms=30_000), approve=['shell'],
                  approver='manual', approve_expire_ms=100)
        self.model.call_script = [[('start_task', {'name': 'fix'}), ('shell', {'command': 'true'})]]
        turn = self.submit(client)
        # Ended by the lapse, as a turn waiting on a verdict is, long before
        # the answered call's own time runs out.
        finished = client.finished(turn)['data']
        self.assertEqual((finished['status'], finished['error']), ('interrupted', 'approval_expired'))
        self.assertEqual(json.loads(self.output(client)),
                         {'error': 'tool_unserved', 'detail': "a later call's gate lapsed before the answer"})

    def test_configure_may_gate_a_tool_it_declares_at_once(self):
        client = self.client('echo')
        client.request('create', bot='Lead', workspace=str(self.path))
        configured = client.request('configure', bot='Lead', declare=[START],
                                    approve=['start_task'], approver='manual')['result']
        self.assertEqual(sorted(configured['changed']), ['declared', 'gates'])

    def test_a_call_that_does_not_fit_is_refused_and_waits_for_nothing(self):
        client = self.lead()
        self.model.call_script = [('start_task', {'name': 1})]
        turn = self.submit(client)
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertEqual(json.loads(self.output(client))['error'], 'invalid_arguments')
        self.assertNotIn('answered_by', self.events(client, turn, 'tool_started')[0])


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class StepTests(ModelFixture):
    """Steps of an answered call, each put to the bot's gates by the call's client."""
    lead, submit, started, answer, output, events = (
        AnsweredToolTests.lead, AnsweredToolTests.submit, AnsweredToolTests.started,
        AnsweredToolTests.answer, AnsweredToolTests.output, AnsweredToolTests.events)

    def script(self, client=None, tool=EXEC, settings=None, **create):
        client = client or self.client('echo', settings=settings)
        client.request('create', bot='Lead', workspace=str(self.path), declare=[tool], **create)
        self.model.call_script = [('exec', {'code': 'run()'})]
        turn = self.submit(client)
        return client, turn, self.started(client, turn)

    def ask(self, client, turn, step='1', name='mcp__chat__send', **params):
        """Send an approve_step without waiting for it; its id, to receive the reply by."""
        client.next_id += 1
        request = dict(id=client.next_id, op='approve_step', bot='Lead', turn=turn, call_id='script-0',
                       step=step, name=name, arguments=params.pop('arguments', {'text': 'hi'}), **params)
        client.process.stdin.write(json.dumps(request) + '\n')
        client.process.stdin.flush()
        return client.next_id

    def reply(self, client, request, timeout=5):
        return client.receive(lambda m: m.get('id') == request, timeout)

    def announced(self, client, turn):
        return client.receive(lambda m: m.get('event') == 'approval_requested' and m.get('turn') == turn)['data']

    def verdict(self, client, turn, call_id, decision='allow', **extra):
        return client.request('answer', bot='Lead', turn=turn, call_id=call_id, request=1,
                              decision=decision, by='test', **extra)

    def test_a_tool_takes_steps_only_when_a_client_answers_it(self):
        client = self.client('echo')
        alone = {k: v for k, v in EXEC.items() if k != 'answered_by'}
        refused = client.request('create', bot='Bad', workspace=str(self.path), declare=[alone])
        self.assertEqual(refused['error'], 'invalid_declare')
        created = client.request('create', bot='Lead', workspace=str(self.path), declare=[EXEC])['result']
        self.assertTrue(created['declared'][0]['steps'])

    def test_a_step_under_no_gate_runs_at_once(self):
        client, turn, _ = self.script()
        step = client.request('approve_step', bot='Lead', turn=turn, call_id='script-0', step='1',
                              name='mcp__docs__read', arguments={'path': 'a'})
        self.assertEqual(step['result'], {'call_id': 'script-0/1', 'decision': 'allow', 'approvals': []})
        self.assertEqual(self.events(client, turn, 'approval_requested'), [])
        self.answer(client, turn, output='read a')
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')

    def test_a_gated_tool_runs_its_call_at_once_and_waits_for_each_step_s_verdict(self):
        client, turn, started = self.script(approve=['exec'], approver='manual')
        # The call itself waits for no verdict.
        self.assertEqual(started['answered_by'], 'sonny')
        self.assertNotIn('approvals', started)
        request = self.ask(client, turn)
        [call] = self.announced(client, turn)['calls']
        self.assertEqual({k: call[k] for k in ('call_id', 'step_of', 'request', 'gates', 'name', 'node')},
                         {'call_id': 'script-0/1', 'step_of': 'script-0', 'request': 1, 'gates': ['manual'],
                          'name': 'mcp__chat__send', 'node': 0})
        [listed] = client.request('approvals', bot='Lead')['result']['approvals']
        self.assertEqual({k: listed[k] for k in ('call_id', 'step_of', 'name', 'arguments')},
                         {'call_id': 'script-0/1', 'step_of': 'script-0', 'name': 'mcp__chat__send',
                          'arguments': {'text': 'hi'}})
        self.assertNotIn('arguments_cut', listed)
        twice = client.request('approve_step', bot='Lead', turn=turn, call_id='script-0', step='1',
                               name='mcp__chat__send', arguments={})
        self.assertEqual(twice['error'], 'step_waiting')
        self.assertEqual(self.verdict(client, turn, 'script-0/1')['result']['pending'], [])
        allowed = self.reply(client, request)['result']
        self.assertEqual((allowed['call_id'], allowed['decision']), ('script-0/1', 'allow'))
        self.assertEqual([(a['tag'], a['by'], a['allow']) for a in allowed['approvals']], [('manual', 'test', True)])
        # A denial's reason is the step's.
        request = self.ask(client, turn, step='2', arguments={'text': 'everyone'})
        self.announced(client, turn)
        self.verdict(client, turn, 'script-0/2', decision='deny', reason='not to everyone')
        denied = self.reply(client, request)['result']
        self.assertEqual((denied['decision'], denied['reason']), ('deny', 'not to everyone'))
        self.assertEqual([(d['call_id'], d['decision']) for d in self.events(client, turn, 'step_decided')],
                         [('script-0/1', 'allow'), ('script-0/2', 'deny')])
        self.assertEqual(client.request('approvals', bot='Lead')['result']['approvals'], [])
        self.answer(client, turn, output='sent once')
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertEqual(self.output(client), 'sent once')
        self.assertEqual(client.request('stats')['result']['approval_requests'], 0)

    def test_a_step_s_wait_stops_its_call_s_time(self):
        # The call has 600 ms and its turn parks at once; its step waits
        # longer than that, and the call's time runs on only after.
        for answered in (True, False):
            with self.subTest(answered=answered):
                self.setUp()
                client, turn, _ = self.script(tool=dict(EXEC, expire_ms=600), settings={'approval_hold_ms': 0},
                                              approve=['exec'], approver='manual')
                began = time.monotonic()
                request = self.ask(client, turn)
                self.announced(client, turn)
                time.sleep(1.0)
                self.verdict(client, turn, 'script-0/1')
                self.assertEqual(self.reply(client, request)['result']['decision'], 'allow')
                if answered:
                    self.assertIn('result', self.answer(client, turn, output='done'))
                    self.assertEqual(client.finished(turn)['data']['status'], 'completed')
                    self.assertEqual(self.output(client), 'done')
                else:
                    # Unanswered, the call runs out of what was left of its time.
                    self.assertEqual(client.finished(turn)['data']['status'], 'completed')
                    self.assertGreater(time.monotonic() - began, 1.0)
                    self.assertEqual(json.loads(self.output(client))['error'], 'tool_unserved')

    def test_a_step_whose_gate_lapses_is_denied_and_its_turn_goes_on(self):
        client, turn, _ = self.script(approve=['exec'], approver='manual', approve_expire_ms=300)
        request = self.ask(client, turn)
        denied = self.reply(client, request)['result']
        self.assertEqual((denied['decision'], denied['reason'], denied['expired']),
                         ('deny', 'not reviewed: no verdict', True))
        self.assertEqual(self.verdict(client, turn, 'script-0/1')['error'], 'no_pending_approval')
        self.answer(client, turn, output='nothing sent')
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')

    def test_an_answer_to_the_call_ends_its_waiting_steps(self):
        client, turn, _ = self.script(approve=['exec'], approver='manual')
        request = self.ask(client, turn)
        self.announced(client, turn)
        self.assertIn('result', self.answer(client, turn, output='gave up'))
        self.assertEqual(self.reply(client, request)['error'], 'no_pending_step')
        [ended] = self.events(client, turn, 'step_decided')
        self.assertEqual((ended['decision'], ended['reason']), ('deny', 'the call was answered first'))
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertEqual(client.request('approvals', bot='Lead')['result']['approvals'], [])

    def test_a_step_goes_with_the_client_that_asked(self):
        daemon = SocketClient(self.binary, self.path / 'state.sqlite', self.url, 'echo')
        self.addCleanup(daemon.close)
        daemon.request('create', bot='Lead', workspace=str(self.path), declare=[EXEC],
                       approve=['exec'], approver='manual')
        self.model.call_script = [('exec', {'code': 'run()'})]
        turn = daemon.request('submit', bot='Lead', request_id='t', prompt='script')['result']['turn']
        deadline = time.monotonic() + 5
        while not daemon.request('tool_calls', tag='sonny')['result']['calls']:
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.01)
        asker = Connection(daemon.socket_path)
        asker.socket.sendall((json.dumps(dict(id=1, op='approve_step', bot='Lead', turn=turn, call_id='script-0',
                                              step='1', name='mcp__chat__send', arguments={})) + '\n').encode())
        while not daemon.request('approvals', bot='Lead')['result']['approvals']:
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.01)
        asker.close()
        while daemon.request('approvals', bot='Lead')['result']['approvals']:
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.01)
        events = daemon.request('events', bot='Lead', after=0, limit=256)['result']['events']
        [ended] = [e['data'] for e in events if e['event'] == 'step_decided']
        self.assertEqual((ended['decision'], ended['reason']), ('deny', 'its client stopped waiting'))
        self.assertIn('result', daemon.request('tool_result', bot='Lead', turn=turn, call_id='script-0', output='ok'))
        self.assertEqual(daemon.finished(turn)['data']['status'], 'completed')

    def test_a_step_is_refused_what_its_call_would_refuse(self):
        client, turn, _ = self.script(tool=EXEC)
        ask = dict(bot='Lead', turn=turn, call_id='script-0', step='1', name='mcp__chat__send', arguments={})
        for change, error in [({'step': 'a b'}, 'invalid_step'), ({'name': ''}, 'invalid_step'),
                              ({'arguments': []}, 'invalid_step'),
                              ({'arguments': {'text': 'x' * 65537}}, 'step_arguments_limit'),
                              ({'call_id': 'nope'}, 'no_pending_call'), ({'bot': 'Other'}, 'turn_not_found')]:
            self.assertEqual(client.request('approve_step', **(ask | change))['error'], error, change)
        self.answer(client, turn, output='ok')
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertEqual(client.request('approve_step', **ask)['error'], 'stale_turn')
        # A tool that takes no steps.
        self.setUp()
        client = self.lead()
        turn = self.submit(client)
        self.started(client, turn)
        self.assertEqual(client.request('approve_step', **(ask | {'turn': turn}))['error'], 'invalid_step')


if __name__ == '__main__':
    unittest.main()
